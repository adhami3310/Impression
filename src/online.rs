use std::{collections::HashMap, fs::DirEntry};

use itertools::Itertools;
use log::warn;

#[derive(Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct DistroRelease {
    pub name: String,
    pub version: Option<String>,
    pub url: String,
    pub variant: Option<String>,
}

pub fn get_osinfodb_url() -> Option<String> {
    let info: serde_json::Value = reqwest::blocking::get("https://db.libosinfo.org/latest.json")
        .ok()?
        .json()
        .ok()?;

    Some(info["release"]["archive"].as_str()?.to_owned())
}

const TWO_YEARS: chrono::Duration = chrono::Duration::days(365 * 2); // Approximation, ignoring leap years

fn matches_must_contains(
    name: &str,
    must_contains: &Option<String>,
    invert_must_contains: &bool,
) -> bool {
    if let Some(must_contains) = must_contains {
        name.contains(must_contains) != *invert_must_contains
    } else {
        true
    }
}

fn is_prerelease(status: &Option<&str>) -> bool {
    matches!(status, Some(x) if *x == "prerelease")
}

fn is_rolling(status: &Option<&str>) -> bool {
    matches!(status, Some(x) if *x == "rolling")
}

fn is_recent(date: &chrono::NaiveDate) -> bool {
    *date + TWO_YEARS >= chrono::offset::Local::now().date_naive()
}

fn is_relevant_for_current_day(date: &Option<chrono::NaiveDate>, status: &Option<&str>) -> bool {
    !is_prerelease(status)
        && match date {
            None => is_rolling(status),
            Some(d) => is_recent(d),
        }
}

fn get_first_child_node_with_tag<'a>(
    parent: &'a roxmltree::Node,
    tag: &str,
) -> Option<roxmltree::Node<'a, 'a>> {
    parent.children().find(|d| d.has_tag_name(tag))
}

fn get_text_of_first_child_node_with_tag<'a>(
    parent: &'a roxmltree::Node,
    tag: &str,
) -> Option<&'a str> {
    get_first_child_node_with_tag(parent, tag).and_then(|n| n.text())
}

fn parse_date(date_str: &str) -> Option<chrono::NaiveDate> {
    let (year, month, day) = date_str
        .split('-')
        .flat_map(|x| x.parse::<u32>().ok())
        .collect_tuple()?;
    chrono::NaiveDate::from_ymd_opt(year as i32, month, day)
}

struct MediaInfo {
    variant_name: String,
    architecture: String,
    url: String,
}

fn get_media_info(
    media_node: &roxmltree::Node,
    variants: &HashMap<String, String>,
    default_name: &str,
) -> Option<MediaInfo> {
    let url = get_text_of_first_child_node_with_tag(media_node, "url")?.to_owned();

    let architecture = media_node.attribute("arch")?.to_owned();

    let variant_name = get_first_child_node_with_tag(media_node, "variant")
        .and_then(|n| n.attribute("id"))
        .and_then(|id| variants.get(id))
        .map(|n| n.to_owned())
        .unwrap_or(default_name.to_owned());

    Some(MediaInfo {
        variant_name,
        architecture,
        url,
    })
}

fn parse_xml_file(
    file: DirEntry,
    must_contains: &Option<String>,
    invert_must_contains: &bool,
) -> Vec<(Option<DistroRelease>, Option<DistroRelease>)> {
    if !file.path().is_file() {
        return vec![];
    }

    let Ok(content) = std::fs::read_to_string(file.path()) else {
        warn!("Failed to read file: {:?}", file.path());
        return vec![];
    };
    let Ok(doc) = roxmltree::Document::parse(&content) else {
        warn!("Failed to parse XML file: {:?}", file.path());
        return vec![];
    };

    let Some(os_element) = doc.descendants().find(|d| d.has_tag_name("os")) else {
        warn!("Couldn't find OS tag inside of file: {:?}", file.path());
        return vec![];
    };

    let release_date =
        get_text_of_first_child_node_with_tag(&os_element, "release-date").and_then(parse_date);

    let release_status = get_text_of_first_child_node_with_tag(&os_element, "release-status");

    if !is_relevant_for_current_day(&release_date, &release_status) {
        return vec![];
    }

    let Some(name) = get_text_of_first_child_node_with_tag(&os_element, "name") else {
        warn!("Couldn't find name tag inside of file: {:?}", file.path());
        return vec![];
    };

    let version = get_text_of_first_child_node_with_tag(&os_element, "version");

    let variants = os_element
        .children()
        .filter(|d| d.has_tag_name("variant"))
        .filter_map(|variant| {
            let id = variant.attribute("id")?;
            let name = get_text_of_first_child_node_with_tag(&variant, "name")?;
            Some((id.to_owned(), name.to_owned()))
        })
        .collect::<HashMap<_, _>>();

    let medias = os_element
        .children()
        .filter(|d| {
            d.has_tag_name("media")
                && (d.attribute("arch") == Some("x86_64") || d.attribute("arch") == Some("aarch64"))
        })
        .flat_map(|media| get_media_info(&media, &variants, name))
        .collect_vec();

    let distros: Vec<(Option<DistroRelease>, Option<DistroRelease>)> = medias
        .into_iter()
        .filter(|media| {
            matches_must_contains(&media.variant_name, must_contains, invert_must_contains)
        })
        .map(|media| {
            let variant_id = variants.iter().find_map(|(k, v)| {
                if media.variant_name == *v {
                    Some(k.to_string())
                } else {
                    None
                }
            });

            (
                media.architecture,
                DistroRelease {
                    name: media.variant_name,
                    version: version.map(str::to_owned),
                    url: media.url,
                    variant: variant_id,
                },
            )
        })
        .map(|(arch, distro)| {
            if arch == "x86_64" {
                (Some(distro), None)
            } else {
                (None, Some(distro))
            }
        })
        .collect();

    distros
}

#[derive(Debug, Default)]
struct DistroInfo {
    amd: Vec<DistroRelease>,
    arm: Vec<DistroRelease>,
}

fn get_releases_for_distro(
    temp_dir: &std::path::Path,
    distro: &str,
    must_contains: &Option<String>,
    invert_must_contains: &bool,
) -> DistroInfo {
    let distro_dir = temp_dir.join(distro);

    let Ok(files) = std::fs::read_dir(temp_dir.join(distro)) else {
        warn!("Failed to read directory: {:?}", &distro_dir);
        return DistroInfo::default();
    };

    let y: (Vec<Option<DistroRelease>>, Vec<Option<DistroRelease>>) = files
        .flatten()
        .flat_map(|file| parse_xml_file(file, must_contains, invert_must_contains))
        .collect();

    let mut amd: HashMap<Option<String>, DistroRelease> = HashMap::new();
    let mut arm: HashMap<Option<String>, DistroRelease> = HashMap::new();

    for distro in y.0.into_iter().flatten() {
        amd.insert(distro.variant.clone(), distro);
    }

    for distro in y.1.into_iter().flatten() {
        arm.insert(distro.variant.clone(), distro);
    }

    DistroInfo {
        amd: amd.into_values().sorted().collect(),
        arm: arm.into_values().sorted().collect(),
    }
}

type DownloadableDistroInfo = (String, Option<String>, bool);

pub fn collect_online_distros(
    latest_url: &str,
    downloadable_distros: &[DownloadableDistroInfo],
) -> Option<(Vec<DistroRelease>, Vec<DistroRelease>)> {
    let temp_dir = glib::user_cache_dir();

    if std::fs::create_dir_all(&temp_dir).is_err() {
        warn!("Failed to create cache directory: {:?}", &temp_dir);
        return None;
    };

    let result_file_path = temp_dir.join("db.tar.xz");

    let Ok(osinfodb_resp) = reqwest::blocking::get(latest_url) else {
        warn!("Failed to download OSInfoDB from {}", latest_url);
        return None;
    };
    let Ok(body) = osinfodb_resp.bytes() else {
        warn!("Failed to get bytes from response");
        return None;
    };

    let Ok(mut out) = std::fs::File::create(&result_file_path) else {
        warn!("Failed to create file: {:?}", &result_file_path);
        return None;
    };

    if std::io::Write::write(&mut out, &body).is_err() {
        warn!("Failed to write to file: {:?}", &result_file_path);
        return None;
    };

    let Ok(status) = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&result_file_path)
        .arg("--directory")
        .arg(&temp_dir)
        .arg("--strip-components=2")
        .arg("--wildcards")
        .args(
            downloadable_distros
                .iter()
                .map(|(name, _, _)| format!("*/os/{name}"))
                .unique(),
        )
        .status()
    else {
        warn!("Failed to execute tar command");
        return None;
    };

    if !status.success() {
        return None;
    }

    use rayon::prelude::*;

    let distros: Vec<DistroInfo> = downloadable_distros
        .into_par_iter()
        .map(|(distro, must_contains, invert_must_contains)| {
            get_releases_for_distro(&temp_dir, distro, must_contains, invert_must_contains)
        })
        .collect();

    let (amd, arm): (Vec<Vec<DistroRelease>>, Vec<Vec<DistroRelease>>) =
        distros.into_iter().map(|d| (d.amd, d.arm)).unzip();

    Some((
        amd.into_iter().flatten().collect::<Vec<_>>(),
        arm.into_iter().flatten().collect::<Vec<_>>(),
    ))
}

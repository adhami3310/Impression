use std::collections::HashMap;

use itertools::Itertools;

#[derive(Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct DistroRelease {
    pub name: String,
    pub version: Option<String>,
    pub url: String,
    pub variant: String,
}

pub fn get_osinfodb_url() -> Option<String> {
    let info: serde_json::Value = reqwest::blocking::get("https://db.libosinfo.org/latest.json")
        .ok()?
        .json()
        .ok()?;

    Some(info["release"]["archive"].as_str()?.to_owned())
}

type DownloadableDistroInfo = (String, Option<String>, bool);

pub fn collect_online_distros(
    latest_url: &str,
    downloadable_distros: &[DownloadableDistroInfo],
) -> Option<(Vec<DistroRelease>, Vec<DistroRelease>)> {
    let temp_dir = glib::user_cache_dir();

    std::fs::create_dir_all(&temp_dir).expect("cannot create temp dir");

    let result_file_path = temp_dir.join("db.tar.xz");

    let osinfodb_resp = reqwest::blocking::get(latest_url).ok()?;
    let body = osinfodb_resp.bytes().ok()?;

    let mut out = std::fs::File::create(&result_file_path).expect("failed to create file");

    std::io::Write::write(&mut out, &body).expect("Failed to download file");

    let status = std::process::Command::new("tar")
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
        .unwrap();

    if !status.success() {
        return None;
    }

    use rayon::prelude::*;

    struct DistroInfo {
        amd: Vec<DistroRelease>,
        arm: Vec<DistroRelease>,
    }

    let distros: Vec<DistroInfo> = downloadable_distros
        .into_par_iter()
        .map(|(distro, must_contains, invert_must_contains)| {
            let files = std::fs::read_dir(temp_dir.join(distro)).unwrap();

            let y: (Vec<Option<DistroRelease>>, Vec<Option<DistroRelease>>) = files
                .flatten()
                .flat_map(|file| {
                    let content = std::fs::read_to_string(file.path()).expect("Cannot read xml");
                    let doc = roxmltree::Document::parse(&content).expect("Cannot parse document");

                    let os_element = doc.descendants().find(|d| d.has_tag_name("os")).unwrap();

                    let release_date = os_element
                        .children()
                        .find(|d| d.has_tag_name("release-date"))
                        .map(|rd| {
                            let (year, month, day) = rd
                                .text()
                                .unwrap()
                                .split('-')
                                .map(|x| x.parse::<u32>().unwrap())
                                .collect_tuple()
                                .unwrap();
                            chrono::NaiveDate::from_ymd_opt(year as i32, month, day).unwrap()
                        });
                    let release_status = os_element
                        .children()
                        .find(|d| d.has_tag_name("release-status"))
                        .map(|rs| rs.text().unwrap().to_string());

                    let name = os_element
                        .children()
                        .find(|d| d.has_tag_name("name"))
                        .unwrap()
                        .text()
                        .unwrap()
                        .to_string();

                    let version = os_element
                        .children()
                        .find(|d| d.has_tag_name("version"))
                        .and_then(|x| x.text().map(|x| x.to_owned()));

                    let variants = os_element
                        .children()
                        .filter(|d| d.has_tag_name("variant"))
                        .map(|d| {
                            (
                                d.attribute("id").unwrap().to_string(),
                                d.descendants()
                                    .find(|n| n.has_tag_name("name"))
                                    .map(|n| n.text().unwrap().to_string())
                                    .unwrap_or(name.clone()),
                            )
                        })
                        .collect::<HashMap<_, _>>();

                    let medias = os_element
                        .children()
                        .filter(|d| {
                            d.has_tag_name("media")
                                && (d.attribute("arch") == Some("x86_64")
                                    || d.attribute("arch") == Some("aarch64"))
                                && d.descendants()
                                    .any(|u| u.has_tag_name("url") && !u.text().unwrap().is_empty())
                        })
                        .map(|m| {
                            (
                                m.children()
                                    .find(|d| d.has_tag_name("variant"))
                                    .map(|n| {
                                        variants.get(n.attribute("id").unwrap()).unwrap().to_owned()
                                    })
                                    .unwrap_or(name.clone()),
                                m.attribute("arch").unwrap(),
                                m.descendants()
                                    .find(|d| d.has_tag_name("url"))
                                    .unwrap()
                                    .text()
                                    .unwrap()
                                    .to_string(),
                            )
                        })
                        .collect_vec();

                    let distros: Vec<(Option<DistroRelease>, Option<DistroRelease>)> = medias
                        .into_iter()
                        .map(|media| {
                            (
                                media.0, // name
                                media.1, // arch
                                media.2, // url
                                release_date,
                                release_status.clone(),
                                version.clone(),
                            )
                        })
                        .filter(|(_, _, _, date, status, _)| {
                            !matches!(status, Some(x) if x == "prerelease")
                                && (date.is_some() || matches!(status, Some(x) if x == "rolling"))
                                && (date.is_none()
                                    || date.unwrap()
                                        + chrono::Duration::try_days(365 * 2)
                                            .expect("duration is overflow")
                                        >= chrono::offset::Local::now().date_naive())
                        })
                        .filter(|(name, _, _, _, _, _)| {
                            if let Some(must_contains) = must_contains {
                                name.contains(must_contains) != *invert_must_contains
                            } else {
                                true
                            }
                        })
                        .filter(|(_, _, _, date, status, _)| {
                            !matches!(status, Some(x) if x == "prerelease")
                                && (date.is_some() || matches!(status, Some(x) if x == "rolling"))
                                && (date.is_none()
                                    || date.unwrap()
                                        + chrono::Duration::try_days(365 * 2)
                                            .expect("duration is overflow")
                                        >= chrono::offset::Local::now().date_naive())
                        })
                        .filter(|(name, _, _, _, _, _)| {
                            if let Some(must_contains) = must_contains {
                                name.contains(must_contains) != *invert_must_contains
                            } else {
                                true
                            }
                        })
                        .map(|(name, arch, url, _, _, version)| {
                            let mut variant = String::new();

                            for (k, v) in &variants {
                                if name == *v {
                                    variant = k.to_string();
                                    break;
                                }
                            }

                            (
                                arch,
                                DistroRelease {
                                    name,
                                    version,
                                    url,
                                    variant,
                                },
                            )
                        })
                        .map(|(arch, distro)| match arch {
                            "x86_64" => (Some(distro), None),
                            _ => (None, Some(distro)),
                        })
                        .collect();

                    distros
                })
                .collect();

            let mut amd: HashMap<String, DistroRelease> = HashMap::new();
            let mut arm: HashMap<String, DistroRelease> = HashMap::new();

            for distro in y.0.into_iter().flatten() {
                if !amd.contains_key(&distro.variant) {
                    amd.insert(distro.variant.to_owned(), distro);
                } else {
                    let ds = amd.get_mut(&distro.variant).unwrap();
                    *ds = distro;
                }
            }

            for distro in y.1.into_iter().flatten() {
                if !arm.contains_key(&distro.variant) {
                    arm.insert(distro.variant.to_owned(), distro);
                } else {
                    let ds = arm.get_mut(&distro.variant).unwrap();
                    *ds = distro;
                }
            }

            DistroInfo {
                amd: amd.into_values().sorted().collect(),
                arm: arm.into_values().sorted().collect(),
            }
        })
        .collect();

    let (amd, arm): (Vec<Vec<DistroRelease>>, Vec<Vec<DistroRelease>>) =
        distros.into_iter().map(|d| (d.amd, d.arm)).unzip();

    Some((
        amd.into_iter().flatten().collect::<Vec<_>>(),
        arm.into_iter().flatten().collect::<Vec<_>>(),
    ))
}

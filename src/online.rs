use std::collections::HashMap;

use itertools::Itertools;

#[derive(thiserror::Error, Debug)]
#[error("Error while extracting compressed file")]
struct TarError {}

pub struct Distro {
    pub name: String,
    pub version: Option<String>,
    pub url: String,
}

pub fn get_osinfodb_url() -> Option<String> {
    let info: serde_json::Value = reqwest::blocking::get("https://db.libosinfo.org/latest.json")
        .ok()?
        .json()
        .ok()?;

    Some(info["release"]["archive"].as_str()?.to_owned())
}

const GOOD_DISTROS: [(&str, &str); 6] = [
    ("archlinux.org", "Arch Linux"),
    ("endlessos.com", "Endless OS"),
    ("fedoraproject.org", "Fedora"),
    ("manjaro.org", "Manjaro"),
    ("opensuse.org", "OpenSUSE"),
    ("ubuntu.com", "Ubuntu"),
];

pub fn collect_online_distros(latest_url: &str) -> Option<(Vec<Distro>, Vec<Distro>)> {
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
            GOOD_DISTROS
                .into_iter()
                .map(|(name, _)| format!("*/os/{name}")),
        )
        .status()
        .unwrap();

    if !status.success() {
        return None;
    }

    use rayon::prelude::*;

    let (amd, arm): (Vec<Option<Distro>>, Vec<Option<Distro>>) = GOOD_DISTROS
        .into_par_iter()
        .map(|(distro, distro_name)| {
            let files = std::fs::read_dir(temp_dir.join(distro)).unwrap();

            let y = files
                .flatten()
                .flat_map(|file| {
                    let content = std::fs::read_to_string(file.path()).unwrap();
                    let doc = roxmltree::Document::parse(&content).unwrap();

                    let os_element = doc.descendants().find(|d| d.has_tag_name("os")).unwrap();

                    let release_date = os_element
                        .children()
                        .find(|d| d.has_tag_name("release-date"))
                        .map(|rd| {
                            let (year, month, day) = rd
                                .text()
                                .unwrap()
                                .to_owned()
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

                    let (amd, arm): (Vec<_>, Vec<_>) =
                        medias.into_iter().partition_map(|(_, a, url)| match a {
                            "x86_64" => itertools::Either::Left(url),
                            _ => itertools::Either::Right(url),
                        });

                    Some((
                        amd.into_iter().next()?,
                        arm.into_iter().next(),
                        release_date,
                        release_status,
                        version,
                    ))
                })
                .filter(|(_, _, date, status, _)| {
                    !matches!(status, Some(x) if x == "prerelease")
                        && (date.is_some() || matches!(status, Some(x) if x == "rolling"))
                        && (date.is_none()
                            || date.unwrap() + chrono::Duration::try_days(365 * 2).expect("duration is overflow")
                                >= chrono::offset::Local::now().date_naive())
                })
                .max_by_key(|(_, _, date, _, _)| date.to_owned())
                .map(|(amd, arm, _, _, version)| {
                    (
                        Distro {
                            name: distro_name.to_owned(),
                            version: version.clone(),
                            url: amd,
                        },
                        arm.map(|arm| Distro {
                            name: distro_name.to_owned(),
                            version: version.clone(),
                            url: arm,
                        }),
                    )
                })
                .unzip();

            (y.0, y.1.flatten())
        })
        .unzip();

    Some((
        amd.into_iter().flatten().collect(),
        arm.into_iter().flatten().collect(),
    ))
}

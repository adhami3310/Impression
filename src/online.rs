use std::collections::HashMap;

use itertools::Itertools;

#[derive(thiserror::Error, Debug)]
#[error("Error while extracting compressed file")]
struct TarError {}

pub fn collect_online_distros() -> anyhow::Result<(
    Vec<(String, Option<String>, String)>,
    Vec<(String, Option<String>, String)>,
)> {
    let libosinfo_files =
        reqwest::blocking::get("https://releases.pagure.org/libosinfo/")?.text()?;

    let hrefs_re = regex::Regex::new(r#"href="([^"]*)""#).unwrap();
    let osinfodb_re = regex::Regex::new(r#"^osinfo-db-\d+\.tar\.xz$"#).unwrap();

    let latest_file = hrefs_re
        .captures_iter(&libosinfo_files)
        .map(|c| (&c[1]).to_string())
        .filter(|s| osinfodb_re.is_match(s))
        .sorted_by_key(|s| {
            s[("osinfo-db-".len())..(s.len() - ".tar.xz".len())]
                .parse::<i32>()
                .unwrap()
        })
        .last()
        .unwrap();

    let latest_file_stem = latest_file[..(latest_file.len() - ".tar.xz".len())].to_string();
    let latest_url = format!("https://releases.pagure.org/libosinfo/{}", latest_file);

    let temp_dir = format!("{}/tmp/", std::env::var("XDG_CACHE_HOME").unwrap());
    let result_file = format!("{}db.tar.xz", temp_dir);
    let result_directory = format!("{}{}/os/", temp_dir, latest_file_stem);
    let good_distros = [
        ("archlinux.org", "Arch Linux"),
        ("endlessos.com", "Endless OS"),
        ("fedoraproject.org", "Fedora"),
        ("manjaro.org", "Manjaro"),
        ("opensuse.org", "OpenSUSE"),
        ("ubuntu.com", "Ubuntu"),
    ];

    let osinfodb_resp = reqwest::blocking::get(latest_url)?;
    let body = osinfodb_resp.bytes()?;
    let mut out = std::fs::File::create(&result_file).expect("failed to create file");

    std::io::Write::write(&mut out, &body).expect("Failed to download file");

    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&result_file)
        .arg("--directory")
        .arg(&temp_dir)
        .status()
        .unwrap();

    if !status.success() {
        return Err(TarError {}.into());
    }

    let mut amd = Vec::new();
    let mut arm = Vec::new();
    for (distro, distro_name) in good_distros {
        let files = std::fs::read_dir(format!("{}{}", result_directory, distro)).unwrap();

        let mut result_amd = None;
        let mut result_arm = None;

        for file in files.flatten() {
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
            if release_status == Some("prerelease".to_owned())
                || release_date.is_none() && release_status != Some("rolling".to_owned())
                || release_date.is_some()
                    && release_date.unwrap() + chrono::Duration::days(365 * 2)
                        < chrono::offset::Local::now().date_naive()
            {
                continue;
            }

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
                .map(|x| x.text().map(|x| x.to_owned()))
                .flatten();

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
                            .map(|n| variants.get(n.attribute("id").unwrap()).unwrap().to_owned())
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

            if !medias.is_empty() {
                let (amd, arm): (Vec<_>, Vec<_>) =
                    medias.into_iter().partition_map(|(name, a, url)| match a {
                        "x86_64" => itertools::Either::Left((name, url)),
                        _ => itertools::Either::Right((name, url)),
                    });

                if let Some((_, url)) = amd.first() {
                    result_amd = match result_amd.to_owned() {
                        None => Some((version.clone(), url.to_owned(), release_date)),
                        Some(x) => match (x.2, release_date) {
                            (Some(pr), Some(nr)) if pr < nr => {
                                Some((version.clone(), url.to_owned(), Some(nr)))
                            }
                            _ => Some(x),
                        },
                    };
                }
                if let Some((_, url)) = arm.first() {
                    result_arm = match result_arm.to_owned() {
                        None => Some((version, url.to_owned(), release_date)),
                        Some(x) => match (x.2, release_date) {
                            (Some(pr), Some(nr)) if pr < nr => {
                                Some((version, url.to_owned(), Some(nr)))
                            }
                            _ => Some(x),
                        },
                    };
                }
            }
        }
        if let Some(result_amd) = result_amd {
            amd.push((distro_name.to_owned(), result_amd.0.clone(), result_amd.1));
            if let Some(result_arm) = result_arm {
                if result_arm.0 == result_amd.0 {
                    arm.push((distro_name.to_owned(), result_arm.0, result_arm.1));
                }
            }
        }
    }

    Ok((amd, arm))
}

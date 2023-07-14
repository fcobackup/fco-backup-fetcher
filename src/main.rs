use chrono::prelude::Utc;
use clap::Parser;
use env_logger::Target;
use log::*;
use serde_json::json;
use std::collections::HashSet;
use std::io::Write;
use std::path::{is_separator, Path, PathBuf};
use std::sync::{Arc, Mutex};
use webdriver_client::messages::NewSessionCmd;
use webdriver_client::{Driver, DriverSession, LocationStrategy};

const FETCHED_AT_PREFIX: &str = "Fetched at: ";

#[derive(clap::Parser)]
#[command(
    name = "FCO Backup",
    author = "FCO Backup <ukfcobackup@gmail.com>",
    version = "0.1.0"
)]
struct App {
    #[arg(long)]
    git_repo: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    DiscoverUnannounced,
    InitialImport,
    PollFeedOnce,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::new()
        .filter(None, LevelFilter::Info)
        .target(Target::Stderr)
        .init();

    let app = App::parse();

    if !app.git_repo.exists() {
        run_git(
            "clone",
            &[
                "git@github.com:fcobackup/fco-backup.git",
                &format!("{}", app.git_repo.display()),
            ],
            &PathBuf::from("/"),
            &[],
        )
        .expect("Git clone failed");
    }

    let countries_root = app.git_repo.join("countries");

    let build_driver = Arc::new(move || {
        retry(
            move || {
                let builder = webdriver_client::chrome::ChromeDriverBuilder::new();
                let chromedriver = builder
                    .spawn()
                    .map_err(|e| format!("Error spawning ChromeDriver: {:?}", e))?;
                chromedriver
                    .session(&NewSessionCmd::default().always_match(
                        "goog:chromeOptions",
                        json!({
                            "args": ["--no-sandbox", "--headless"],
                        }),
                    ))
                    .map(|d| Arc::new(d))
                    .map_err(|e| format!("Error starting browser: {:?}", e))
            },
            || {},
        )
    });
    let driver = RestartableDriver::new(build_driver);
    match app.command {
        Command::DiscoverUnannounced => {
            discover_unannounced(&driver, &countries_root, &app.git_repo)
                .expect("Error discovering unannounced");
        }
        Command::InitialImport => {
            fetch_all(&driver, &countries_root, &app.git_repo, "Initial import")
                .expect("Error fetching all");
        }
        Command::PollFeedOnce => {
            poll_atom(&driver, &countries_root, &app.git_repo).expect("Error polling feed");
        }
    }
}

fn poll_atom(
    driver: &RestartableDriver,
    countries_root: &Path,
    git_repo: &Path,
) -> Result<(), String> {
    let (new_entries, all_are_new) = get_new_atom_entries(git_repo)?;

    if new_entries.len() == 0 {
        return Ok(());
    }

    if all_are_new || has_duplicates(&new_entries) {
        return fetch_all(
            &driver,
            &countries_root,
            &git_repo,
            "Missed some updates as they happened, catching up",
        );
    }

    for entry in new_entries {
        let summary = parse_summary(&entry);
        let country = Country {
            name: entry.title().as_str().to_owned(),
            url: entry
                .links()
                .iter()
                .find(|link| link.mime_type() == Some("text/html"))
                .unwrap()
                .href()
                .to_owned(),
        };
        let country_root = countries_root.join(country.dir_name()?);
        if country_root.exists() {
            git_rm(&git_repo, &country_root)?;
        }
        let dir = fetch_country_dir(&driver, &countries_root, &country)?;
        git_add(&git_repo, &dir)?;
        git_commit(&git_repo, &format!("{}: {}", country.name, summary))?;
    }
    git_push(&git_repo)?;
    Ok(())
}

fn get_new_atom_entries(git_repo: &Path) -> Result<(Vec<atom_syndication::Entry>, bool), String> {
    let feed = retry(
        || {
            let response = reqwest::blocking::get("https://www.gov.uk/foreign-travel-advice.atom")
                .map_err(|e| format!("Error fetching atom feed: {:?}", e))?;
            if !response.status().is_success() {
                return Err(format!(
                    "Got status {} ({}) for atom feed",
                    response.status(),
                    response.status().as_u16()
                ));
            }
            atom_syndication::Feed::read_from(std::io::BufReader::new(response))
                .map_err(|e| format!("Error parsing atom feed: {:?}", e))
        },
        || {},
    )?;

    let last_known_timestamp = get_last_known_timestamp(&git_repo)?;

    let new_entries = feed
        .entries()
        .iter()
        .map(|e| e.clone())
        .rev()
        .filter_map(|entry| {
            let updated = entry.updated();
            if updated > &last_known_timestamp {
                Some(Ok(entry))
            } else {
                None
            }
        })
        .collect::<Result<Vec<atom_syndication::Entry>, String>>()?;

    let len = new_entries.len();
    Ok((new_entries, feed.entries().len() == len))
}

fn parse_summary(entry: &atom_syndication::Entry) -> String {
    match entry.summary() {
        Some(summary) => match sxd_document::parser::parse(summary) {
            Ok(summary_xpath) => {
                let summary_document = summary_xpath.as_document();
                match sxd_xpath::evaluate_xpath(
                    &summary_document,
                    "/*[local-name()='div']/*[local-name()='p']",
                ) {
                    Ok(value) => value.string(),
                    Err(_) => summary.as_str().to_owned(),
                }
            }
            Err(_) => summary.as_str().to_owned(),
        },
        None => "[No summary]".to_owned(),
    }
}

fn fetch_all(
    driver: &RestartableDriver,
    countries_root: &Path,
    git_repo: &Path,
    reason: &str,
) -> Result<(), String> {
    if countries_root.exists() {
        git_rm(&git_repo, &countries_root)?;
    }

    let country_list = retry(|| list_countries(&driver.get()?), || driver.restart())
        .map_err(|e| format!("Error listing countries: {:?}", e))?;
    for country in country_list {
        let dir = fetch_country_dir(&driver, &countries_root, &country)?;
        git_add(&git_repo, &dir)?;
    }
    git_commit(&git_repo, &reason)?;
    git_push(&git_repo)?;
    Ok(())
}

fn discover_unannounced(
    driver: &RestartableDriver,
    countries_root: &Path,
    git_repo: &Path,
) -> Result<(), String> {
    poll_atom(driver, countries_root, git_repo)?;

    if countries_root.exists() {
        git_rm(&git_repo, &countries_root)?;
    }

    let country_list = retry(|| list_countries(&driver.get()?), || driver.restart())
        .map_err(|e| format!("Error listing countries: {:?}", e))?;
    for country in country_list {
        let dir = fetch_country_dir(&driver, &countries_root, &country)?;
        git_add(&git_repo, &dir)?;
    }

    if get_new_atom_entries(git_repo)?.0.len() > 0 {
        error!("Changed were published while discovering unannounced changes");
    }

    let output_bytes = run_git("diff", &["--name-only", "--cached"], git_repo, &[])?;

    let message = if output_bytes.len() == 0 {
        "No unannounced changes discovered"
    } else {
        "Changes discovered which weren't announced on the atom feed"
    };

    git_commit(&git_repo, message)?;
    git_push(&git_repo)?;
    Ok(())
}

fn has_duplicates(entries: &Vec<atom_syndication::Entry>) -> bool {
    let urls = entries
        .iter()
        .filter_map(|entry| {
            entry
                .links()
                .iter()
                .find(|link| link.mime_type() == Some("text/html"))
                .map(|link| link.href())
        })
        .collect::<HashSet<_>>();
    urls.len() < entries.len()
}

fn git_add(current_dir: &Path, to_add: &Path) -> Result<(), String> {
    run_git("add", &[to_add], &current_dir, &[]).map(|_| ())
}

fn git_rm(current_dir: &Path, to_delete: &Path) -> Result<(), String> {
    run_git(
        "rm",
        &["-r", &to_delete.to_string_lossy().to_string()],
        &current_dir,
        &[],
    )
    .map(|_| ())
}

fn git_commit(current_dir: &Path, message: &str) -> Result<(), String> {
    run_git(
        "commit",
        &[
            "--author=FCO Backup <ukfcobackup@gmail.com>",
            "--allow-empty",
            "-m",
            &format!(
                "{}\n\n{}{}",
                message,
                FETCHED_AT_PREFIX,
                Utc::now().format("%FT%TZ")
            ),
        ],
        current_dir,
        &["user.name=FCO Backup", "user.email=ukfcobackup@gmail.com"],
    )
    .map(|_| ())
}

fn git_push(current_dir: &Path) -> Result<(), String> {
    run_git(
        "push",
        &["origin", "master"],
        current_dir,
        &["user.name=FCO Backup", "user.email=ukfcobackup@gmail.com"],
    )
    .map(|_| ())
}

fn run_git<S: AsRef<std::ffi::OsStr>>(
    command: &str,
    args: &[S],
    dir: &Path,
    config_args: &[&str],
) -> Result<Vec<u8>, String> {
    let mut c = std::process::Command::new("git");
    for config in config_args {
        c.arg("-c").arg(config);
    }
    let output = c
        .arg(command)
        .args(args)
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("Error running git {}: {:?}", command, e))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!("Error running git {}: Bad exit code", command))
    }
}

fn get_last_known_timestamp(
    git_repo: &Path,
) -> Result<chrono::DateTime<chrono::offset::FixedOffset>, String> {
    let output = std::process::Command::new("git")
        .args(&["log", "--format=%B", "-n1", "HEAD"])
        .current_dir(&git_repo)
        .output()
        .map_err(|e| format!("Error running git status: {:?}", e))?;
    if !output.status.success() {
        return Err(format!(
            "Error running git log: Bad exit code. stderr: {:?}",
            String::from_utf8(output.stderr)
        ));
    }
    let commit_message = String::from_utf8(output.stdout)
        .map_err(|e| format!("commit message was not utf8: {:?}", e))?;
    let commit_message_lines = commit_message.split("\n");
    for line in commit_message_lines.collect::<Vec<_>>().iter().rev() {
        if line.starts_with(FETCHED_AT_PREFIX) {
            match chrono::DateTime::parse_from_rfc3339(&line[FETCHED_AT_PREFIX.len()..]) {
                Ok(date) => return Ok(date),
                Err(_) => {}
            }
        }
    }
    Err("Unknown timestamp".to_string())
}

fn fetch_country_dir(
    driver: &RestartableDriver,
    countries_root: &Path,
    country: &Country,
) -> Result<PathBuf, String> {
    info!("Fetching country {}", country.name);
    let pages = retry(
        || fetch_country(&driver.get()?, &country.url),
        || driver.restart(),
    )
    .map_err(|e| format!("Error fetching {}: {:?}", country.name, e))?;
    let dir = countries_root.join(&country.dir_name()?);
    std::fs::remove_dir_all(&dir)
        .or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })
        .map_err(|e| format!("Error removing directory {:?}: {:?}", dir, e))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Error creating directory {:?}: {:?}", dir, e))?;
    for page in pages {
        let file_path = dir.join(page.file_name());
        std::fs::File::create(&file_path)
            .and_then(|mut file| file.write_all(page.content.as_bytes()))
            .map_err(|e| format!("Error write file {:?}: {:?}", file_path, e))?;
    }
    Ok(dir)
}

struct Country {
    pub name: String,
    pub url: String,
}

impl Country {
    pub fn dir_name(&self) -> Result<&str, String> {
        let dir_name = self.url.split("/").last().unwrap();
        if dir_name == "." || dir_name == ".." {
            return Err(format!("Bad path: {}", dir_name));
        }
        for c in dir_name.chars() {
            if is_separator(c) {
                return Err(format!("Bad path: {}", dir_name));
            }
        }
        Ok(dir_name)
    }
}

fn list_countries(driver: &Arc<DriverSession>) -> Result<Vec<Country>, String> {
    driver
        .go("https://www.gov.uk/foreign-travel-advice")
        .map_err(|e| format!("Error getting countries list: {:?}", e))?;
    let links = driver
        .find_elements(".countries-list a", LocationStrategy::Css)
        .map_err(|e| format!("Error getting links in country list: {:?}", e))?;
    links
        .iter()
        .map(|link| {
            Ok(Country {
                name: link
                    .text()
                    .map_err(|e| format!("Error getting link text: {:?}", e))?,
                url: property(driver, link, "href")
                    .map_err(|e| format!("Error getting href: {:?}", e))?,
            })
        })
        .collect()
}

struct TitleAndContent {
    pub title: String,
    pub content: String,
}

impl TitleAndContent {
    pub fn file_name(&self) -> String {
        let mut filename = String::new();
        for part in self.title.to_lowercase().split_whitespace() {
            let delim = if &filename == "" { "" } else { "-" };
            filename = format!(
                "{}{}{}",
                filename,
                delim,
                part.replace(".", "_").replace("/", "_")
            );
        }
        filename
    }
}

fn fetch_country(driver: &Arc<DriverSession>, url: &str) -> Result<Vec<TitleAndContent>, String> {
    driver
        .go(url)
        .map_err(|e| format!("Error getting url {}: {:?}", url, e))?;

    let mut pages_to_contents = Vec::new();
    let mut links_to_follow = Vec::new();

    let pages = driver
        .find_elements(
            "nav[aria-label=\"Travel advice pages\"] li",
            LocationStrategy::Css,
        )
        .map_err(|e| format!("Error finding travel advice pages on page {}: {:?}", url, e))?;
    for page in pages {
        let links = page
            .find_elements("a", LocationStrategy::Css)
            .map_err(|e| format!("Error finding links: {:?}", e))?;
        match links.len() {
            0 => pages_to_contents.push(fetch_page(&driver)?),
            1 => links_to_follow.push(
                property(driver, links.get(0).unwrap(), "href")
                    .map_err(|e| format!("Error getting href of link on page {}: {:?}", url, e))?,
            ),
            _ => {
                warn!("Warning: Found more than one link in a table of contents, picking first.");
                links_to_follow.push(
                    property(driver, links.get(0).unwrap(), "href").map_err(|e| {
                        format!("Error getting href of link on page {}: {:?}", url, e)
                    })?,
                )
            }
        };
    }
    for link in links_to_follow {
        driver
            .go(&link)
            .map_err(|e| format!("Error going to page {}: {:?}", url, e))?;
        pages_to_contents.push(fetch_page(&driver)?);
    }

    Ok(pages_to_contents)
}

fn property(
    session: &webdriver_client::DriverSession,
    element: &webdriver_client::Element,
    property: &str,
) -> Result<String, webdriver_client::Error> {
    // ChromeDriver doesn't currently support getting element properties:
    // https://bugs.chromium.org/p/chromedriver/issues/detail?id=1936
    let cmd = webdriver_client::messages::ExecuteCmd {
        script: format!("return arguments[0].{}", property),
        args: vec![element.reference().expect("Getting element reference")],
    };
    session
        .execute(cmd)
        .map(|v| v.as_str().unwrap_or_default().to_owned())
}

fn fetch_page(driver: &DriverSession) -> Result<TitleAndContent, String> {
    let content_elements = driver.find_elements(".govuk-govspeak", LocationStrategy::Css);
    let content_texts = content_elements
        .map_err(|e| format!("Error finding text: {:?}", e))?
        .into_iter()
        .map(|elem| elem.text())
        .collect::<Result<Vec<String>, _>>()
        .map_err(|e| format!("Error getting text: {:?}", e))?;
    let mut content = content_texts.join("\n\n");
    content += "\n";
    Ok(TitleAndContent {
        title: driver
            .find_element(".part-title", LocationStrategy::Css)
            .and_then(|elem| elem.text())
            .map_err(|e| format!("Error getting title {:?}", e))?,
        content: content,
    })
}

struct RestartableDriver {
    session: Arc<Mutex<Option<Result<Arc<DriverSession>, String>>>>,
    build_driver: Arc<dyn Fn() -> Result<Arc<DriverSession>, String>>,
}

impl RestartableDriver {
    pub fn new(
        build_driver: Arc<dyn Fn() -> Result<Arc<DriverSession>, String>>,
    ) -> RestartableDriver {
        RestartableDriver {
            session: Arc::new(Mutex::new(None)),
            build_driver: build_driver,
        }
    }

    pub fn get(&self) -> Result<Arc<DriverSession>, String> {
        {
            let maybe_s = self.session.lock().unwrap();
            match *maybe_s {
                Some(ref s) => return s.clone(),
                None => {}
            }
        }
        self.restart();
        self.get()
    }

    pub fn restart(&self) {
        let build_driver = self.build_driver.clone();
        let driver_result = build_driver();
        let mut session = self.session.lock().unwrap();
        *session = Some(driver_result);
    }
}

fn retry<Value, Error: std::fmt::Debug, Do: Fn() -> Result<Value, Error>, OnError: Fn()>(
    f: Do,
    on_error: OnError,
) -> Result<Value, String> {
    let mut errors = vec![];
    for _ in 0..2 {
        match f() {
            Ok(value) => return Ok(value),
            Err(err) => {
                warn!("Retrying because of error {:?}", err);
                on_error();
                errors.push(err)
            }
        }
    }
    f().map_err(|e| {
        errors.push(e);
        format!("Giving up after 3 attempts: {:?}", errors)
    })
}

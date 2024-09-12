use chrono::prelude::Utc;
use clap::Parser;
use env_logger::Target;
use eyre::{bail, eyre, Context, Result};
use log::*;
use std::collections::HashSet;
use std::ffi::OsString;
use std::future::Future;
use std::io::Write;
use std::path::{is_separator, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use thirtyfour::{By, DesiredCapabilities, WebDriver};
use tokio::time;

const FETCHED_AT_PREFIX: &str = "Fetched at: ";

#[derive(clap::Parser)]
#[command(
    name = "FCO Backup",
    author = "FCO Backup <ukfcobackup@gmail.com>",
    version = "0.1.0"
)]
struct Args {
    #[command(subcommand)]
    command: Command,

    #[arg(long, default_value = "git@github.com:fcobackup/fco-backup.git")]
    git_remote: String,

    #[arg(long)]
    local_git_repo_path: PathBuf,

    /// Command used to start a chromedriver process on port 9515.
    #[arg(long, required = true)]
    chromedriver_start_command: Vec<OsString>,
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

    let args = {
        let mut args = Args::parse();
        if !args.local_git_repo_path.is_absolute() {
            let working_directory =
                std::env::current_dir().expect("Failed to get working directory");
            args.local_git_repo_path = working_directory.join(args.local_git_repo_path);
        }
        args
    };

    if !args.local_git_repo_path.exists() {
        run_git(
            "clone",
            &[
                OsString::from(args.git_remote).as_os_str(),
                args.local_git_repo_path.as_os_str(),
            ],
            &PathBuf::from("/"),
            &[],
        )
        .expect("Git clone failed");
    }

    let countries_root = args.local_git_repo_path.join("countries");

    std::process::Command::new(&args.chromedriver_start_command[0])
        .args(args.chromedriver_start_command.iter().skip(1))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start chromedriver");

    time::sleep(Duration::from_secs(2)).await;

    let mut capabilities = DesiredCapabilities::chrome();
    // Required to run in docker.
    capabilities.add_chrome_arg("--no-sandbox").expect("Failed to add --no-sandbox arg");
    let driver = WebDriver::new("http://127.0.0.1:9515", capabilities)
        .await
        .expect("Failed to start WebDriver instance");

    info!("Started WebDriver");

    match args.command {
        Command::DiscoverUnannounced => {
            discover_unannounced(&driver, &countries_root, &args.local_git_repo_path)
                .await
                .expect("Error discovering unannounced");
        }
        Command::InitialImport => {
            fetch_all(
                &driver,
                &countries_root,
                &args.local_git_repo_path,
                "Initial import",
            )
            .await
            .expect("Error fetching all");
        }
        Command::PollFeedOnce => {
            poll_atom(&driver, &countries_root, &args.local_git_repo_path)
                .await
                .expect("Error polling feed");
        }
    }
}

async fn poll_atom(driver: &WebDriver, countries_root: &Path, git_repo: &Path) -> Result<()> {
    let (new_entries, all_are_new) = get_new_atom_entries(git_repo).await?;

    if new_entries.len() == 0 {
        return Ok(());
    }

    if all_are_new || has_duplicates(&new_entries) {
        return fetch_all(
            &driver,
            &countries_root,
            &git_repo,
            "Missed some updates as they happened, catching up",
        )
        .await;
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
        let dir = fetch_country_dir(&driver, &countries_root, &country).await?;
        git_add(&git_repo, &dir)?;
        git_commit(&git_repo, &format!("{}: {}", country.name, summary))?;
    }
    git_push(&git_repo)?;
    Ok(())
}

async fn get_new_atom_entries(git_repo: &Path) -> Result<(Vec<atom_syndication::Entry>, bool)> {
    let feed = retry(|| async {
        let response = reqwest::get("https://www.gov.uk/foreign-travel-advice.atom")
            .await
            .context("Error fetching atom feed")?;
        if !response.status().is_success() {
            bail!(
                "Got status {} ({}) for atom feed",
                response.status(),
                response.status().as_u16()
            );
        }
        let bytes = response.bytes().await.context("Reading atom feed")?;
        atom_syndication::Feed::read_from(std::io::BufReader::new(bytes.as_ref()))
            .context("Error parsing atom feed")
    })
    .await?;

    let last_known_timestamp = get_last_known_timestamp(&git_repo)?;

    let new_entries: Vec<_> = feed
        .entries()
        .iter()
        .map(|e| e.clone())
        .rev()
        .filter_map(|entry| {
            let updated = entry.updated();
            if updated > &last_known_timestamp {
                Some(entry)
            } else {
                None
            }
        })
        .collect();

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

async fn fetch_all(
    driver: &WebDriver,
    countries_root: &Path,
    git_repo: &Path,
    reason: &str,
) -> Result<()> {
    if countries_root.exists() {
        git_rm(&git_repo, &countries_root)?;
    }

    let country_list = retry(|| async { list_countries(driver).await })
        .await
        .context("Error listing countries")?;
    for country in country_list {
        let dir = fetch_country_dir(&driver, &countries_root, &country).await?;
        git_add(&git_repo, &dir)?;
    }
    git_commit(&git_repo, &reason)?;
    git_push(&git_repo)?;
    Ok(())
}

async fn discover_unannounced(
    driver: &WebDriver,
    countries_root: &Path,
    git_repo: &Path,
) -> Result<()> {
    poll_atom(driver, countries_root, git_repo).await?;

    if countries_root.exists() {
        git_rm(&git_repo, &countries_root)?;
    }

    let country_list = retry(|| async { list_countries(driver).await })
        .await
        .context("Error listing countries")?;
    for country in country_list {
        let dir = fetch_country_dir(&driver, &countries_root, &country).await?;
        git_add(&git_repo, &dir)?;
    }

    if get_new_atom_entries(git_repo).await?.0.len() > 0 {
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

fn git_add(current_dir: &Path, to_add: &Path) -> Result<()> {
    run_git("add", &[to_add], &current_dir, &[]).map(|_| ())
}

fn git_rm(current_dir: &Path, to_delete: &Path) -> Result<()> {
    run_git(
        "rm",
        &["-r", &to_delete.to_string_lossy().to_string()],
        &current_dir,
        &[],
    )
    .map(|_| ())
}

fn git_commit(current_dir: &Path, message: &str) -> Result<()> {
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

fn git_push(current_dir: &Path) -> Result<()> {
    run_git(
        "push",
        &["origin", "main"],
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
) -> Result<Vec<u8>> {
    let mut c = std::process::Command::new("git");
    for config in config_args {
        c.arg("-c").arg(config);
    }
    let output = c
        .arg(command)
        .args(args)
        .current_dir(&dir)
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("Error running git {}", command))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        bail!("Error running git {}: Bad exit code", command)
    }
}

fn get_last_known_timestamp(
    git_repo: &Path,
) -> Result<chrono::DateTime<chrono::offset::FixedOffset>> {
    let output = std::process::Command::new("git")
        .args(&["log", "--format=%B", "-n1", "HEAD"])
        .current_dir(&git_repo)
        .output()
        .context("Error running git status")?;
    if !output.status.success() {
        bail!(
            "Error running git log: Bad exit code. stderr: {:?}",
            String::from_utf8(output.stderr)
        );
    }
    let commit_message = String::from_utf8(output.stdout).context("commit message was not utf8")?;
    let commit_message_lines = commit_message.split("\n");
    for line in commit_message_lines.collect::<Vec<_>>().iter().rev() {
        if line.starts_with(FETCHED_AT_PREFIX) {
            match chrono::DateTime::parse_from_rfc3339(&line[FETCHED_AT_PREFIX.len()..]) {
                Ok(date) => return Ok(date),
                Err(_) => {}
            }
        }
    }
    bail!("Unknown timestamp")
}

async fn fetch_country_dir(
    driver: &WebDriver,
    countries_root: &Path,
    country: &Country,
) -> Result<PathBuf> {
    info!("Fetching country {}", country.name);
    let pages = retry(|| async { fetch_country(driver, &country.url).await })
        .await
        .with_context(|| format!("Error fetching {}", country.name))?;
    let dir = countries_root.join(&country.dir_name()?);
    std::fs::remove_dir_all(&dir)
        .or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })
        .with_context(|| format!("Error removing directory {:?}", dir))?;
    std::fs::create_dir_all(&dir).with_context(|| format!("Error creating directory {:?}", dir))?;
    for page in pages {
        let file_path = dir.join(page.file_name());
        std::fs::File::create(&file_path)
            .and_then(|mut file| file.write_all(page.content.as_bytes()))
            .with_context(|| format!("Error write file {:?}", file_path))?;
    }
    Ok(dir)
}

#[derive(Debug)]
struct Country {
    pub name: String,
    pub url: String,
}

impl Country {
    pub fn dir_name(&self) -> Result<&str> {
        let dir_name = self.url.split("/").last().unwrap();
        if dir_name == "." || dir_name == ".." {
            bail!("Bad path: {dir_name}");
        }
        for c in dir_name.chars() {
            if is_separator(c) {
                bail!("Bad path: {dir_name}");
            }
        }
        Ok(dir_name)
    }
}

async fn list_countries(driver: &WebDriver) -> Result<Vec<Country>> {
    driver
        .goto("https://www.gov.uk/foreign-travel-advice")
        .await
        .context("Error getting countries list")?;
    let links = driver
        .find_all(By::Css(".countries-list a"))
        .await
        .context("Error getting links in country list")?;
    let mut countries = Vec::with_capacity(links.len());
    for link in links {
        countries.push(Country {
            name: link.text().await.context("Error getting link text")?,
            url: link
                .prop("href")
                .await
                .context("Error getting href")?
                .ok_or_else(|| eyre!("No href on country link"))?,
        })
    }
    Ok(countries)
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

async fn fetch_country(driver: &WebDriver, url: &str) -> Result<Vec<TitleAndContent>> {
    driver
        .goto(url)
        .await
        .with_context(|| format!("Error getting url {}", url))?;

    let mut pages_to_contents = Vec::new();
    let mut links_to_follow = Vec::new();

    let pages = driver
        .find_all(By::Css("nav[aria-label=\"Travel advice pages\"] li"))
        .await
        .with_context(|| format!("Error finding travel advice pages on page {}", url))?;
    for page in pages {
        let links = page
            .find_all(By::Css("a"))
            .await
            .context("Error finding links")?;
        match links.as_slice() {
            [] => pages_to_contents.push(fetch_page(&driver).await?),
            [link, rest @ ..] => {
                if !rest.is_empty() {
                    warn!(
                        "Warning: Found more than one link in a table of contents, picking first."
                    );
                }
                links_to_follow.push(
                    link.prop("href")
                        .await
                        .with_context(|| format!("Error getting href of link on page {}", url))?
                        .ok_or_else(|| {
                            eyre!("Link didn't have href property when fetching country")
                        })?,
                )
            }
        }
    }
    for link in links_to_follow {
        driver
            .goto(&link)
            .await
            .with_context(|| format!("Error going to page {url}"))?;
        pages_to_contents.push(fetch_page(&driver).await?);
    }

    Ok(pages_to_contents)
}

async fn fetch_page(driver: &WebDriver) -> Result<TitleAndContent> {
    let content_elements = driver
        .find_all(By::Css(".govuk-govspeak"))
        .await
        .context("Error finding text")?;
    let mut content_texts = Vec::with_capacity(content_elements.len());
    for content_element in content_elements {
        let text = content_element
            .text()
            .await
            .context("Error getting text of content element")?;
        content_texts.push(text);
    }
    let mut content = content_texts.join("\n\n");
    content += "\n";

    let title = driver
        .find(By::Css(".govuk-heading-l"))
        .await
        .context("Error getting title")?
        .text()
        .await
        .context("Error getting title's text")?;
    Ok(TitleAndContent { title, content })
}

async fn retry<Value, Fut: Future<Output = Result<Value>>, Do: Fn() -> Fut>(
    f: Do,
) -> Result<Value> {
    let mut errors = vec![];
    for _ in 0..2 {
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                warn!("Retrying because of error {:?}", err);
                errors.push(err)
            }
        }
    }
    f().await.map_err(|e| {
        errors.push(e);
        eyre::eyre!("Giving up after 3 attempts: {:?}", errors)
    })
}

// copyright: B1 Systems GmbH <info@b1-systems.de>, 2022
// license: GPLv3+, http://www.gnu.org/licenses/gpl-3.0.html
// author: Daniel Poelzleithner <poelzleithner@b1-systems.de>, 2022

/// Parses the email from file or stdin. Extracts all pdf attachments
/// and uploads the file to a cloud storage or local path
extern crate base64;
extern crate mailparse;

#[macro_use]
extern crate log;

use anyhow::Result;
use backoff::backoff::Backoff;
use clap::{arg, command, Parser};
use mailparse::*;
use tera::{Context, Value, Tera};
use std::collections::HashMap;
use std::fmt::Display;
use std::io::prelude::*;
use std::path::PathBuf;
use std::str::FromStr;
use std::string::*;
use std::{fs::File};
use tokio;

// lazy_static! {
//     static ref DEFAULT_EXTRACT_MIME: MimeArguments = MimeArguments(vec![
//         "application/pdf".to_string(),
//     ]);
// }

const DEFAULT_EXTRACT_MIMES: [&'static str; 1] = ["application/pdf"];
const UNKNOWN_USER_DEFAULT: &'static str = "_UNKNOWN";
const UNKNOWN_FROM_DEFAULT: &'static str = "UNKNOWN";
const DEFAULT_PATH_TEMPLATE: &'static str = "{{user | lower}}/{{file_name | escape_filename}}";
const DEFAULT_IMAP_FOLDER: &'static str = "{{user | lower}}.new";


/// Mimetype argument list
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(transparent)]
struct MimeArguments(Vec<String>);

impl Display for MimeArguments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.join(";"))
    }
}

impl From<Vec<String>> for MimeArguments {
    fn from(v: Vec<String>) -> Self {
        MimeArguments(v)
    }
}

impl Default for MimeArguments {
    fn default() -> Self {
        let inner: Vec<String> = DEFAULT_EXTRACT_MIMES.iter().map(|x| x.to_string()).collect();
        Self(inner)
    }
}

impl FromStr for MimeArguments {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let accepted: Vec<String> = s.split(";").map(|x| x.to_owned()).collect();
        Ok(MimeArguments(accepted))
    }
}

impl Into<clap::builder::OsStr> for MimeArguments {
    fn into(self) -> clap::builder::OsStr {
        self.0.join(";").into()
    }
}

// /// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Folder name for unknown user
    #[arg(long, default_value_t = {"_unknown".to_string()})]
    unknown_user: String,

    #[arg(long, default_value_t={MimeArguments::default()})]
    accepted_mimetypes: MimeArguments,

    #[arg(default_value_t= {"-".into()}, help = "File to extract")]
    file: String,

    #[arg(long, short, action=clap::ArgAction::Count, default_value_t=1, help = "Increase verbosity")]
    verbose: u8,

    #[arg(long, short, action=clap::ArgAction::SetTrue, help = "Silence all output")]
    quiet: bool,

    // Output options
    /// Local path to save extensions to
    #[arg(long, env="LOCAL_PATH")]
    local_path: Option<PathBuf>,

    /// Store extensions at webdav target
    #[arg(long, env="HTTP_PATH")]
    http_path: Option<String>,

    /// Store extensions at webdav target
    #[arg(long, env, default_value_t = DEFAULT_PATH_TEMPLATE.to_owned(), help = "template for file output")]
    output_template: String,

    /// Imap target folder
    #[arg(long, env, default_value_t = DEFAULT_IMAP_FOLDER.to_owned(), help = "IMAP output folder")]
    imap_template: String,
}

/// Returns the given text that is safe to use as a filename.
/// The returned filename is safe on all major platforms.
pub fn escape_filename(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = tera::try_get_value!("escape_filename", "value", String, value);

    let mut output = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            token if token.is_control()  => output.push_str("_"),
            '<' => output.push_str("_"),
            '>' => output.push_str("_"),
            ':' => output.push_str("_"),
            '\"' => output.push_str("_"),
            '/' => output.push_str("__"),
            '\\' => output.push_str("__"),
            '|' => output.push_str("#"),
            '?' => output.push_str("#"),
            '*' => output.push_str("#"),
            _ => output.push(c),
        }
    }
    Ok(Value::String(output))
}

fn create_template_engine() -> Tera {
    let mut tt = Tera::empty();
    tt.register_filter("escape_filename", escape_filename);
    tt
}

/// Extract all files from a ParsedMail that match the selected mime types
/// Returns a list of extracted file names and the number of export errors
async fn extract_files(
    parsed: &ParsedMail<'_>,
    args: &Args,
    user: &Option<String>,
) -> Result<(Vec<String>, u32)> {


    let mut files = Vec::new();
    let mut errors = 0;

    let mut unknown = 0;
    let from_ = parsed.headers.get_first_value("from").unwrap_or(UNKNOWN_FROM_DEFAULT.to_owned());

    // output template context
    //let mut tt = TinyTemplate::new();
    //tt.add_template("output", &args.output_template)?;
    let mut tt = create_template_engine();


    let output = create_object_store(args)?;



    let mut retry_backoff = backoff::ExponentialBackoff::default();
    for subpart in parsed.subparts.iter() {
        //let mimes = args.accepted_mimetypes.0;
        if args.accepted_mimetypes.0.contains(&subpart.ctype.mimetype) {
            let content = &subpart.get_content_disposition();
            if content.disposition == DispositionType::Attachment {
                println!("{:?}", content.params);
                let filename: String = content
                    .params
                    .get("filename")
                    .map(|x| x.clone())
                    .unwrap_or_else(|| {
                        unknown += 1;
                        format!("attachment-{}", unknown)
                    });

                // let context = TemplateContext {
                //     user:  user.to_owned().unwrap_or(args.unknown_user.clone()),
                //     file_name: filename,
                //     from: from_.clone(),
                //     file_size: 0,
                // };
                let muser = match user {
                    Some(x) => x,
                    None => &args.unknown_user,
                };

                let mut context = Context::new();
                context.insert("user", muser);
                context.insert("file_name", &filename);
                context.insert("from", &from_);


                // let path = format!(
                //     "{}/{}",
                //     user.as_ref()
                //         .unwrap_or(&args.unknown_user),
                //     &filename
                // );
                //let path = tt.render("output", &context)?;
                let rendered = tt.render_str(&args.output_template, &context);
                let path = match rendered {
                    Ok(x) => x,
                    Err(e) => {
                        log::error!("Error rendering output path: {}", e);
                        errors += 1;
                        continue;
                    }
                };

                // write to backend store
                log::info!("Save file: {}", &path);
                let body = subpart.get_body_raw();
                if let Ok(body_vec) = body {
                    loop {
                        let success = output.put(&path.clone().into(), body_vec.clone().into()).await;
                        match success {
                            Ok(_) => {
                                files.push(path);
                                retry_backoff.reset();
                                break;
                            },
                            Err(e) => {
                                errors += 1;
                                let wait = retry_backoff.next_backoff();
                                log::warn!("Error storing file: {}", e);
                                match wait {
                                    Some(wait) => {
                                        log::info!("Retry in: {} seconds", wait.as_secs());
                                        tokio::time::sleep(wait).await
                                    },
                                    None => {
                                        log::error!("Maximum number of retries reached.");
                                        errors += 1;
                                        break;
                                    }
                                }
                            }
                        };
                    }
                } else {
                    log::warn!("Can't get body of attachment: {}", body.err().unwrap());
                    errors += 1;
                }
            }
        }
    }

    Ok((files, errors))
}

/// Extracts the target username from the message argument
/// It tries:
/// 1. Extract username from the to field: anything+[USERNAME]@something
/// 2. If To and From domains match, use the from username
pub fn extract_user(message: &ParsedMail) -> Option<String> {
    // check the to to field for result
    if let Some(to) = message.headers.get_first_value("to") {
        if let Ok(parsed_addr) = mailparse::addrparse(&to) {
            if parsed_addr.len() > 0 {
                match &parsed_addr[0] {
                    MailAddr::Single(info) => {
                        let v: Vec<&str> = info.addr.split_terminator('+').collect();
                        if v.len() == 2 {
                            // substring before @
                            let only_name: Vec<&str> = v[1].split_terminator('@').collect();
                            if only_name.len() == 2 {
                                return Some(only_name[0].to_string());
                            }
                        }
                    }
                    _ => unimplemented!(),
                }
            }
        }
        // extract user from from field if domains match
        if let Some(from_) = message.headers.get_first_value("from") {
            let parsed_from = mailparse::addrparse(&from_);
            let parsed_to = mailparse::addrparse(&to);
            if let (Ok(from_list), Ok(to_list)) = (parsed_to, parsed_from) {
                if from_list.len() > 0 && to_list.len() > 0 {
                    // extract domain names
                    let from_domain = match &from_list[0] {
                        MailAddr::Single(info) => {
                            info.addr.rsplit('@').nth(0)
                        }
                        _ => None,
                    };
                    let to_domain = match &to_list[0] {
                        MailAddr::Single(info) => {
                            info.addr.rsplit('@').nth(0)
                        }
                        _ => None,
                    };
                    // in case both domains match, extract from username
                    if let (Some(to_domain), Some(from_domain)) = (to_domain, from_domain) {
                        // extract the user from
                        if to_domain == from_domain {
                            if let Some(user) = match &from_list[0] {
                                MailAddr::Single(info) => {
                                    info.addr.split('@').nth(0).and_then(|addr| addr.split("+").nth(0))
                                }
                                _ => None,
                            } {
                                return Some(user.to_string());
                            }
                        }
                    }
                } else {
                    log::error!("To and From are empty");
                }
            }
        }
    };
    None
}

/// Creates the object_store to save objects to.
fn create_object_store(args: &Args) -> Result<Box<dyn object_store::ObjectStore>> {
    if let Some(local_path) = &args.local_path {
        return Ok(Box::new(
            object_store::local::LocalFileSystem::new_with_prefix(local_path)?,
        ));
    } else if let Some(http_path) = &args.http_path {
        let store = object_store::http::HttpBuilder::new().
            with_url(http_path).
            build()?;
        return Ok(Box::new(store));
    }
    anyhow::bail!("Please specify storage backend");
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    //    /// Folder name for unknown user
    //    #[arg(long, default_value_t = {"_unknown".to_string()})]
    //    unknown_user: String,

    //    /// Path to save extensions to
    //    #[arg(long)]
    //    local_path: Option<PathBuf>,

    //    #[arg(long, default_value_t = DEFAULT_EXTRACT_MIME)]
    //    accepted_mimetypes: MimeArguments,

    //    #[arg(default_value_t = {"-".into()})]
    //    file: String,
    // let matches = command!() // requires `cargo` feature
    //     .arg(
    //         arg!(--unknown_user <NAME> "Name if user can't be identified")
    //             .default_value(UNKNOWN_USER_DEFAULT)
    //             .required(false),
    //     )
    //     .arg(
    //         arg!(
    //             --local_path <PATH> "Name if user can't be identified"
    //         )
    //         .value_parser(value_parser!(PathBuf)),
    //     )
    //     .arg(
    //         arg!(
    //             --accepted_mimetypes <ACCEPTED_MIMETYPES> "List of mimetypes to accept"
    //         )
    //         .required(false)
    //         .default_value(extract_mimes)
    //         .value_parser(MimeArguments::from_str),
    //     )
    //     .arg(
    //         arg!(
    //             [FILE] "File to parse, - for stdin"
    //         )
    //         .required(false)
    //         .default_value("-"),
    //     )
    //     .arg(
    //         Arg::new("verbosity")
    //             .short('v')
    //             .action(clap::ArgAction::Count)
    //             .help("Increase message verbosity"),
    //     )
    //     .arg(Arg::new("quiet").short('q').help("Silence all output"))
    //     .get_matches();

    // let verbose = matches.get_count("verbosity") as usize;
    // let quiet = matches.get_one::<String>("quiet").is_some();

    stderrlog::new()
        .module(module_path!())
        .quiet(args.quiet)
        .verbosity(args.verbose as usize)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();
    // let cfg = App::new("prog")
    //         .arg(Arg::with_name("unknown_user")
    //                 .long("unknown_user")
    //                 .takes_value(true)
    //                 .value_name("NAME")
    //                 .help("Name when user can't be determined"));

    let mut content: String = String::new();

    // if matches.get_flag("debug") {
    //     log::set_max_level(log::LevelFilter::Debug);
    // } else {
    //     log::set_max_level(log::LevelFilter::Info);
    // }

    let file_name = &args.file;
    if file_name == "-" {
        let res = std::io::stdin().lock().read_to_string(&mut content);
        if res.is_err() {
            log::error!("Can't read stdin: {}", res.err().unwrap());
        }
    } else {
        let mut file = File::open(&file_name).unwrap();
        let res = file.read_to_string(&mut content);
        if res.is_err() {
            log::error!("Can't read stdin: {}", res.err().unwrap());
        }
    }
    let parsed = parse_mail(content.as_bytes());

    let mut move_failed = false;

    match parsed {
        Ok(message) => {

            let user = extract_user(&message);
            let res = extract_files(&message, &args, &user).await;

            match &res {
                Ok((files, errors)) => {
                    log::info!(
                        "Found {} files for user {}",
                        files.len(),
                        user.clone().unwrap_or(UNKNOWN_USER_DEFAULT.into())
                    );
                    if errors > &0 {
                        move_failed = true;
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    move_failed = true;
                }
            };
        },
        Err(e) => {
            log::error!("Error, can't parse mime email: {}", e);
            move_failed = true;
        }
    };


    // copy message to imap
}



#[cfg(test)]
mod tests{
    use super::*;

    #[test]
    fn test_user_extraction() {
        macro_rules! test_user {
            ($m:expr, None) => {
                    let msg1 = parse_mail($m.as_ref()).unwrap();
                    assert_eq!(extract_user(&msg1),
                    None);
            };
            ($m:expr, $exp:expr) => {
                        let msg1 = parse_mail($m.as_ref()).unwrap();
                        assert_eq!(extract_user(&msg1),
                            Some($exp.to_string()));
                        };
        }
        test_user!(r#"
            From: test@example.com
            To: office@example.com
            "#,
            "test");
        test_user!(r#"
            From: test@test.com
            To: office@example.com
            "#,
            None);
        test_user!(r#"
            From: test+user1@test.com
            To: office@example.com
            "#,
            "user1");
        test_user!(r#"
            From: foo+user1@example.com
            To: office@example.com
            "#,
            "foo");
    }
    #[test]
    fn test_escape_fn() {
        let mut tt = create_template_engine();
        let mut context = Context::new();
        context.insert("file_name",  "sH\\itty/fIl name.xml");
        assert_eq!(
            tt.render_str("{{ file_name | escape_filename}}", &context).unwrap(),
            "sH__itty__fIl name.xml".to_owned());
    }
}

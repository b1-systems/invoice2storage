// copyright: B1 Systems GmbH <info@b1-systems.de>, 2022
// license: GPLv3+, http://www.gnu.org/licenses/gpl-3.0.html
// author: Daniel Poelzleithner <poelzleithner@b1-systems.de>, 2022

/// Parses the email from file or stdin. Extracts all pdf attachments
/// and uploads the file to a cloud storage or local path
extern crate base64;
extern crate mailparse;

#[macro_use]
extern crate log;

use anyhow::{Result, anyhow, bail, Context};
use backoff::backoff::Backoff;
use clap::{arg, command, Parser};
use maildir::Maildir;
use mailparse::*;
use tera::{Value, Tera};
use tokio::sync::oneshot::error;
use url::Url;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::io::prelude::*;
use std::path::{PathBuf, Path};
use std::process::ExitCode;
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
const DEFAULT_MAIL_TEMPLATE: &'static str = "{{user | lower}}.{% if errors %}new{% else %}done{% endif %}";
const DEFAULT_MAIL_FLAGS: &'static str = "";
const ERROR_MAIL_FLAGS: &'static str = "F";
const FALLBACK_MAIL_TARGET: &'static str = "";
const IMAP_INBOX_PREFIX: &'static str = "INBOX";

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

/// A email processor to extract email attachments and store them on a storage backend.
/// like webdav, directory, s3, ...
/// 
/// All templates are in the tera template. https://tera.netlify.app/
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// user name for unknown user
    #[arg(long, default_value_t = {UNKNOWN_USER_DEFAULT.to_string()})]
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
    #[arg(long, help = "Ignore tls/https errors")]
    insecure: bool,

    /// Overwrite the detected user with specified
    #[arg(long)]
    overwrite_user: Option<String>,

    /// Store extensions at webdav target
    #[arg(long, help = "Pipe mail to stdout. Useful when used as a pipe filter")]
    stdout: bool,

    /// Target path for generated file
    #[arg(long, env, default_value_t = DEFAULT_PATH_TEMPLATE.to_owned(), help = "template for file output path")]
    output_template: String,

    /// Maildir output
    #[arg(long, env="MAILDIR_PATH", help = "Maildir folder to save messages to, instead of imap")]
    maildir_path: Option<PathBuf>,

    /// Store extensions at webdav target
    #[arg(long, env="IMAP_URL", help = "IMAP connection url. imaps://user:password@host")]
    imap_url: Option<String>,

    /// Imap target folder
    #[arg(long, env, default_value_t = DEFAULT_MAIL_TEMPLATE.to_owned(), help = "Mail template folder")]
    mail_template: String,
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
    let mut tt: Tera = Default::default();
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

                let mut context = tera::Context::new();
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
        let options = object_store::ClientOptions::new()
            .with_allow_http(true)
            .with_allow_invalid_certificates(args.insecure);
        let store = object_store::http::HttpBuilder::new()
            .with_url(http_path)
            .with_client_options(options)
            .build()?;
        return Ok(Box::new(store));
    }
    anyhow::bail!("Please specify storage backend");
}

// fn get_account_info() -> AccountConfig {
//     AccountConfig {
//         email: "test@localhost".into(),
//         display_name: Some("invoice2storage".into()),
//         ..Default::default()
//     }
// }

/// Stores mail in a maildir target
fn store_to_maildir(path: &Path, content: &str, target: &str, flags: &str) -> Result<()> {
    // write message to maildir backend
    // let mut backend = BackendBuilder::build(&ac, &backend_config)?;
    log::debug!("Target maildir folder: {}", target);
    // let exists = backend.folder_list()
    //     .map(|x| x.0.into_iter()
    //         .filter(|f| {println!("{}", &f.name); f.name == target}).count());
    
    // create folder if there is no match or error
    let new_path = if target.len() > 0 {
        let dirname = format!(".{}", target);
        let pname = PathBuf::from_str(&dirname)?;
        path.join(pname)
    } else {
        path.to_owned()
    };
    log::debug!("Target path {}", new_path.display());
    let md = Maildir::from(new_path);
    let _ = md.create_dirs()?;

    let id = md.store_new(content.as_bytes())?;
    let res = md.move_new_to_cur(&id);
    let _add_flags = md.add_flags(&id, flags);
    // if exists.map(|x| x == 0).unwrap_or(true)  {
    //     log::info!("Target folder does not exist, creating");
    //     let new_path = if target.len() > 0 {
    //         let dirname = format!(".{}", target);
    //         let pname = PathBuf::from_str(&dirname)?;
    //         path.join(pname)
    //     } else a
    //         path.to_owned()
    //     };
    //     let md = Maildir::from(new_path);
    //     let _ = md.create_dirs()?;
    //     // match backend.folder_add(target) {
    //     //     Ok(_) => (),
    //     //     Err(err) => {
    //     //         anyhow::bail!("Error creating folder: {}", err);
    //     //     }
    //     // }
    // }
    //let res = backend.email_add(target, content.as_bytes(), flags);
    match res {
        Ok(_) => {
            log::info!("Maildir message was stored: {}", &id);
            Ok(())
        },
        Err(e) => {
            log::info!("Error storing to Maildir: {}", e);
            anyhow::bail!(e);
        }
    }
}

/// Stores mail in a maildir target
async fn store_to_imap(server: &str, content: &str, target: &str, flags: &str) -> Result<()> {
    let conn_info = Url::parse(server).context("Can't parse imap target URL")?;

    let domain = conn_info.domain().ok_or_else(|| anyhow!("IMAP server domain is empty"))?;
    let port = conn_info.port().unwrap_or(993);

    let mut imap_session = if conn_info.scheme().to_lowercase() == "imaps" {
        let tls = async_native_tls::TlsConnector::new();
        // we pass in the domain twice to check that the server's TLS
        // certificate is valid for the domain we're connecting to.
        let client = async_imap::connect((domain, port), domain, tls).await?;
    
        // the client we have here is unauthenticated.
        // to do anything useful with the e-mails, we need to log in
        if conn_info.username().len() == 0 {
            log::error!("IMAP requires a login user and password");
            return bail!("IMAP user & password required")
        }
        let pass = conn_info.password().ok_or(anyhow!("IMAP password not set"))?;
        let imap_session = client
            .login(conn_info.username(), pass)
            .await
            .map_err(|e| e.0)?;
        imap_session
    } else {
        bail!("Only imaps is supported")
    };

    let mbox_name = if target.len() == 0 {
        IMAP_INBOX_PREFIX.to_string()
    } else {
        format!("{}.{}", IMAP_INBOX_PREFIX, target)
    };
    
    // we want to fetch the first email in the INBOX mailbox
    let select = imap_session.select(&mbox_name).await;
    if let Err(err) = &select {
        println!("Err {:?}", err);
        log::info!("Creating target folder:  {}", &mbox_name);

        let create_result = imap_session.create(&mbox_name).await;
        if let Err(err) = create_result {
            log::error!("Can't create target folder: {} {}", &mbox_name, err);
            return Err(anyhow!("Can't create target folder: {}", err));
        }
        let select = imap_session.select(&mbox_name).await;
    }
    if select.is_err() {
        log::error!("Creating select imap folder:  {}", &mbox_name);
        bail!("Creating select imap folder:  {}", &mbox_name)
    }

    let append = imap_session.append(&mbox_name, content).await;
    
    if let Err(err) = append {
        log::error!("Can't append to target folder: {} {}", &mbox_name, err);
        bail!("Can't append to target folder: {}", err);
    }

    Ok(())
}

/// Checks Args for configured targets and stores mail there
async fn store_message(args: &Args, content: &str, target: &str, flags: &str) -> Result<()> {
    if let Some(maildir) = &args.maildir_path {
        // wrap in async runner
        return store_to_maildir(maildir.as_path(), content, target, flags);
    }
    if let Some(imap_url) = &args.imap_url {
        // wrap in async runner
        return store_to_imap(imap_url, content, target, flags).await;
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();

    // configure logging
    stderrlog::new()
        .module(module_path!())
        .quiet(args.quiet)
        .verbosity(args.verbose as usize)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();


    let mut content: String = String::new();

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

    let mut user: String = args.unknown_user.to_owned();
    let mut user_found = false;
    let mut has_errors = false;
    let mut path_name_context = tera::Context::new();

    match parsed {
        Ok(message) => {

            if let Some(overwrite_user) = &args.overwrite_user {
                user.clone_from(overwrite_user);
                user_found = true;
            } else if let Some(extracted_user) = extract_user(&message) {
                user = extracted_user;
                user_found = true;
            };
            let user_option = if user_found {
                Some(user.clone())
            } else {
                None
            };
            let res = extract_files(&message, &args, &user_option).await;


            match &res {
                Ok((files, errors)) => {
                    path_name_context.insert("errors", errors);
                    path_name_context.insert("files", files);
                    log::info!(
                        "Found {} files for user {}",
                        files.len(),
                        &user
                    );
                    if errors > &0 {
                        has_errors = true;
                    }
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    has_errors = true;
                }
            };
        },
        Err(e) => {
            log::error!("Error, can't parse mime email: {}", e);
            has_errors = true;
        }
    };
    let _ = path_name_context.insert("has_errors", &has_errors);
    let _ = path_name_context.insert("user", &user);
    // calculate the output folder name
    let mut template = create_template_engine();
    let target_folder = template.render_str(&args.mail_template, &path_name_context)
        .unwrap_or_else(|err| {
            log::error!("Can´t render output folder path: {}. Template was: '{}'", &err, &args.mail_template);
            log::error!("Fallback folder '{}'", &FALLBACK_MAIL_TARGET);
            FALLBACK_MAIL_TARGET.to_owned()
        });
    let flags = if has_errors { ERROR_MAIL_FLAGS } else { DEFAULT_MAIL_FLAGS };

    // backoff::
    let mut retry_backoff = backoff::ExponentialBackoff::default();
    loop {
        log::debug!("Store message");
        let store_result = store_message(&args, &content, &target_folder, flags).await;
        match store_result {
            Ok(_x) => {
                break;
            },
            Err(e) => {
                let wait = retry_backoff.next_backoff();
                log::warn!("Error storing mail: {}", e);
                match wait {
                    Some(wait) => {
                        log::info!("Retry in: {} seconds", wait.as_secs());
                        tokio::time::sleep(wait).await
                    },
                    None => {
                        log::error!("Maximum number of retries reached.");
                        break;
                    }
                }
            }
        };
    }

    // pip message if requested
    if args.stdout {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
    
        let res = handle.write_all(content.as_bytes());
        if res.is_err() {
            log::error!("Can't write to stdout: {}", res.err().unwrap());
            return ExitCode::from(1);
        }
    };
    ExitCode::SUCCESS
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
        let mut context = tera::Context::new();
        context.insert("file_name",  "sH\\itty/fIl name.xml");
        assert_eq!(
            tt.render_str("{{ file_name | escape_filename}}", &context).unwrap(),
            "sH__itty__fIl name.xml".to_owned());
    }
}

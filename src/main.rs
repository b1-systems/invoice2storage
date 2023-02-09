// copyright: B1 Systems GmbH <info@b1-systems.de>, 2022
// license: GPLv3+, http://www.gnu.org/licenses/gpl-3.0.html
// author: Daniel Poelzleithner <poelzleithner@b1-systems.de>, 2022-2023

/// Parses the email from file or stdin. Extracts all pdf attachments
/// and uploads the file to a cloud storage or local path
extern crate base64;
extern crate mailparse;

extern crate log;

use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use clap::{arg, command, Parser};
use clap_serde_derive::ClapSerde;
use imap::types::Flag;
use maildir::Maildir;
use mailparse::*;
use resolve_path::PathResolveExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::fs::File;
use std::io::{prelude::*, BufReader};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::string::*;
use tera::{Tera, Value};
use tokio;
use url::Url;

// lazy_static! {
//     static ref DEFAULT_EXTRACT_MIME: MimeArguments = MimeArguments(vec![
//         "application/pdf".to_string(),
//     ]);
// }

const DEFAULT_CONFIG_FILE: &'static str = "~/.config/invoice2storage/config.toml";
const DEFAULT_EXTRACT_MIMES: [&'static str; 1] = ["application/pdf"];
const UNKNOWN_USER_DEFAULT: &'static str = "_UNKNOWN";
const UNKNOWN_FROM_DEFAULT: &'static str = "UNKNOWN";
const DEFAULT_OUTPUT_TEMPLATE: &'static str = "{{user | lower}}/{{file_name | escape_filename}}";
const DEFAULT_MAIL_TEMPLATE: &'static str =
    "{{user | lower}}.{% if errors %}new{% else %}done{% endif %}";
const DEFAULT_FILE_NAME: &'static str = "-";
const DEFAULT_ERROR_FLAGS: &'static str = "\\Flag";
const DEFAULT_SUCCESS_FLAGS: &'static str = "";
//const DEFAULT_MAIL_FLAGS: &'static str = "";
//const ERROR_MAIL_FLAGS: &'static str = "F";
//const DEFAULT_MAIL_FLAGS: &'static [Flag] = &[Flag::Seen];
//const ERROR_MAIL_FLAGS: &'static [Flag] = &[Flag::Flagged];
const FALLBACK_MAIL_TARGET: &'static str = "";
const IMAP_INBOX_PREFIX: &'static str = "INBOX";

/// Mimetype argument list
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
        let inner: Vec<String> = DEFAULT_EXTRACT_MIMES
            .iter()
            .map(|x| x.to_string())
            .collect();
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

/// Verifier does not verify anything. Used with --insecure mode
struct NoCertificateVerification {}
impl rustls::client::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> std::result::Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::Certificate,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::Certificate,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::client::WebPkiVerifier::verification_schemes()
    }

    fn request_scts(&self) -> bool {
        true
    }
}

/// A email processor to extract email attachments and store them on a storage backend.
/// like webdav, directory, s3, ...
///
/// All templates are in the tera template. https://tera.netlify.app/
#[derive(Parser, Deserialize, ClapSerde)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// user name for unknown user
    #[arg(long, default_value = {DEFAULT_CONFIG_FILE.to_string()}, help = "Config file to load")]
    config_file: std::path::PathBuf,

    /// Rest of arguments
    #[command(flatten)]
    pub config: <Config as ClapSerde>::Opt,
}

#[derive(ClapSerde, Debug)]
pub struct Config {
    /// user name for unknown user
    #[arg(long, default_value = {UNKNOWN_USER_DEFAULT.to_string()})]
    unknown_user: String,

    #[arg(long, default_value={MimeArguments::default()})]
    accepted_mimetypes: MimeArguments,

    #[arg(default_value= {DEFAULT_FILE_NAME.to_string()}, help = "File to extract")]
    file: String,

    #[arg(long, short, action=clap::ArgAction::Count, default_value="1", help = "Increase verbosity")]
    verbose: u8,

    #[arg(long, short, action=clap::ArgAction::SetTrue, help = "Silence all output")]
    quiet: bool,

    // Output options
    /// Local path to save extensions to
    #[arg(long, env = "LOCAL_PATH")]
    local_path: Option<PathBuf>,

    /// Store extensions at webdav target
    #[arg(long, env = "HTTP_PATH")]
    http_path: Option<String>,

    /// Store extensions at webdav target
    #[arg(long, action=clap::ArgAction::SetTrue, help = "Ignore tls/https errors")]
    insecure: bool,

    /// Overwrite the detected user with specified
    #[arg(long)]
    overwrite_user: Option<String>,

    /// Store extensions at webdav target
    #[arg(long, help = "Pipe mail to stdout. Useful when used as a pipe filter")]
    stdout: bool,

    /// Target path for generated file
    #[arg(long, env, default_value = {DEFAULT_OUTPUT_TEMPLATE.to_owned()}, help = "template for file output path")]
    output_template: String,

    /// Maildir output
    #[arg(
        long,
        env = "MAILDIR_PATH",
        help = "Maildir folder to save messages to, instead of imap"
    )]
    maildir_path: Option<PathBuf>,

    /// Store extensions at webdav target
    #[arg(
        long,
        env = "IMAP_URL",
        help = "IMAP connection url. imaps://user:password@host"
    )]
    imap_url: Option<String>,

    /// Imap target folder
    #[arg(long, env, default_value = DEFAULT_MAIL_TEMPLATE.to_owned(), help = "Mail template folder")]
    mail_template: String,

    /// Flags in success case
    // default_value_t = {DEFAULT_MAIL_FLAGS.to_owned().map(|x| x.to_string()) }
    #[arg(long, env, default_value = DEFAULT_SUCCESS_FLAGS.to_owned(), help = "Mail flags in success case")]
    success_flags: Vec<String>,

    #[arg(long, env, default_value = DEFAULT_ERROR_FLAGS, help = "Mail flags in error cases")]
    error_flags: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ProcessResult {
    num_errors: u32,
    files: Vec<String>,
    user: Option<String>,
    mailbox: Option<String>,
}

impl ProcessResult {
    pub fn is_success(&self) -> bool {
        self.num_errors == 0
    }
}

impl Display for ProcessResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Process result: {}. {} files processed, user: {}, mailbox: {}",
            if self.is_success() {
                "success"
            } else {
                "failure"
            },
            self.files.len(),
            self.user.clone().unwrap_or("[unknown]".into()),
            self.mailbox.clone().unwrap_or("[unknown]".into())
        )
    }
}

/// Returns the given text that is safe to use as a filename.
/// The returned filename is safe on all major platforms.
pub fn escape_filename(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let s = tera::try_get_value!("escape_filename", "value", String, value);

    let mut output = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            token if token.is_control() => output.push_str("_"),
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
    config: &Config,
    user: &Option<String>,
) -> Result<(Vec<String>, u32)> {
    let mut files = Vec::new();
    let mut errors = 0;

    let mut unknown = 0;
    let from_ = parsed
        .headers
        .get_first_value("from")
        .unwrap_or(UNKNOWN_FROM_DEFAULT.to_owned());

    // output template context
    //let mut tt = TinyTemplate::new();
    //tt.add_template("output", &args.output_template)?;
    let mut tt = create_template_engine();

    let output = create_object_store(config)?;

    let accepted_mimetypes = &config.accepted_mimetypes;
    let muser = match user {
        Some(x) => x.clone(),
        None => config.unknown_user.clone(),
    };

    let mut retry_backoff = backoff::ExponentialBackoff::default();
    for subpart in parsed.subparts.iter() {
        //let mimes = args.accepted_mimetypes.0;
        if accepted_mimetypes.0.contains(&subpart.ctype.mimetype) {
            let content = &subpart.get_content_disposition();
            if content.disposition == DispositionType::Attachment {
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

                let mut context = tera::Context::new();
                context.insert("user", &muser);
                context.insert("file_name", &filename);
                context.insert("from", &from_);

                // let path = format!(
                //     "{}/{}",
                //     user.as_ref()
                //         .unwrap_or(&args.unknown_user),
                //     &filename
                // );
                //let path = tt.render("output", &context)?;
                let rendered = tt.render_str(&config.output_template, &context);
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
                        let success = output
                            .put(&path.clone().into(), body_vec.clone().into())
                            .await;
                        match success {
                            Ok(_) => {
                                files.push(path);
                                retry_backoff.reset();
                                break;
                            }
                            Err(e) => {
                                errors += 1;
                                let wait = retry_backoff.next_backoff();
                                log::warn!("Error storing file: {}", e);
                                match wait {
                                    Some(wait) => {
                                        log::info!("Retry in: {} seconds", wait.as_secs());
                                        tokio::time::sleep(wait).await
                                    }
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
            if let (Ok(from_list), Ok(to_list)) = (parsed_from, parsed_to) {
                if from_list.len() > 0 && to_list.len() > 0 {
                    // extract domain names
                    let from_domain = match &from_list[0] {
                        MailAddr::Single(info) => info.addr.rsplit('@').nth(0),
                        _ => None,
                    };
                    let to_domain = match &to_list[0] {
                        MailAddr::Single(info) => info.addr.rsplit('@').nth(0),
                        _ => None,
                    };
                    // in case both domains match, extract from username
                    if let (Some(to_domain), Some(from_domain)) = (to_domain, from_domain) {
                        // extract the user from the from part
                        if to_domain == from_domain {
                            if let Some(user) = match &from_list[0] {
                                MailAddr::Single(info) => info
                                    .addr
                                    .split('@')
                                    .nth(0)
                                    .and_then(|addr| addr.split("+").nth(0)),
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
fn create_object_store(config: &Config) -> Result<Box<dyn object_store::ObjectStore>> {
    if let Some(local_path) = &config.local_path {
        return Ok(Box::new(
            object_store::local::LocalFileSystem::new_with_prefix(local_path)?,
        ));
    } else if let Some(http_path) = &config.http_path {
        let allow_insecure = config.insecure;
        let options = object_store::ClientOptions::new()
            .with_allow_http(true)
            .with_allow_invalid_certificates(allow_insecure);
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

/// Transforms a list of imap flags to maildir flag
fn flags2maildir(flags: &Vec<String>) -> String {
    let mut rv = String::new();
    let imap_flags = flags2imap(flags);
    // FIXME: support for dovecot-keywords file
    // https://doc.dovecot.org/admin_manual/mailbox_formats/maildir/
    for flag in imap_flags {
        let add = match flag {
            Flag::Answered => Some("A".to_owned()),
            Flag::Seen => Some("S".to_owned()),
            Flag::Flagged => Some("F".to_owned()),
            Flag::Deleted => Some("T".to_owned()),
            Flag::Draft => Some("D".to_owned()),
            Flag::Recent => None,
            Flag::MayCreate => None,
            Flag::Custom(x) => {
                if x.len() != 1 || !x.chars().all(|x| x.is_lowercase()) {
                    log::warn!("Only one letter raw flags are currently supported in maildir. Ignoring flag");
                    None
                } else {
                    Some(x.to_string())
                }
            }
        };
        if let Some(add) = add {
            rv.push_str(&add);
        }
    }
    rv
}

/// Transforms a list of imap flags to maildir flag
fn flags2imap(flags: &Vec<String>) -> Vec<Flag> {
    // FIXME: support for dovecot-keywords file
    // https://doc.dovecot.org/admin_manual/mailbox_formats/maildir/
    flags.iter().map(|x| Flag::from(x.as_ref())).collect()
}

/// Stores mail in a maildir target
fn store_to_maildir(path: &Path, content: &str, target: &str, flags: &Vec<String>) -> Result<()> {
    // write message to maildir backend
    // let mut backend = BackendBuilder::build(&ac, &backend_config)?;
    log::debug!("Target maildir folder: {}", target);
    // let exists = backend.folder_list()
    //     .map(|x| x.0.into_iter()
    //         .filter(|f| {println!("{}", &f.name); f.name == target}).count());

    // create folder if there is no match or error
    let new_path = if target.len() > 0 {
        let dirname = format!(".{}", target);
        let path_name = PathBuf::from_str(&dirname)?;
        path.join(path_name)
    } else {
        path.to_owned()
    };
    log::debug!("Target path {}", new_path.display());
    let md = Maildir::from(new_path);
    let _ = md.create_dirs()?;

    let id = md.store_new(content.as_bytes())?;
    let res = md.move_new_to_cur(&id);
    let maildir_flags = flags2maildir(flags);

    let _add_flags = md.add_flags(&id, &maildir_flags);
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
            log::info!(
                "Maildir message was stored. Mailbox: {} Id: {} ",
                &target,
                &id
            );
            Ok(())
        }
        Err(e) => {
            log::info!("Error storing to Maildir: {}", e);
            anyhow::bail!(e);
        }
    }
}

/// Stores mail in a maildir target
fn store_to_imap(
    server: &str,
    content: &str,
    target: &str,
    flags: &Vec<String>,
    insecure: bool,
) -> Result<()> {
    let conn_info = Url::parse(server).context("Can't parse imap target URL")?;

    let domain = conn_info
        .domain()
        .ok_or_else(|| anyhow!("IMAP server domain is empty"))?;
    let port = conn_info.port().unwrap_or(993);

    let mut imap_session = if conn_info.scheme().to_lowercase() == "imaps" {
        //let tls = async_native_tls::TlsConnector::new();
        // we pass in the domain twice to check that the server's TLS
        // certificate is valid for the domain we're connecting to.
        //let client = async_imap::connect((domain, port), domain, tls).await?;
        let stream = TcpStream::connect((domain, port))?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().expect("could not load platform certs")
        {
            if let Err(err) = root_store.add(&rustls::Certificate(cert.0)) {
                log::warn!(
                    "Got error while importing some native certificates: {:?}",
                    err
                );
            }
        }

        let mut options = rustls::client::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        if insecure {
            options
                .dangerous()
                .set_certificate_verifier(std::sync::Arc::new(NoCertificateVerification {}));
        }

        let client_connection = rustls::ClientConnection::new(options.into(), domain.try_into()?)?;
        let tls_stream = rustls::StreamOwned::new(client_connection, stream);

        let client = imap::Client::new(tls_stream);

        // the client we have here is unauthenticated.
        // to do anything useful with the e-mails, we need to log in
        if conn_info.username().len() == 0 {
            log::error!("IMAP requires a login user and password");
            bail!("IMAP user & password required")
        }
        let pass = conn_info
            .password()
            .ok_or(anyhow!("IMAP password not set"))?;
        let imap_session = client.login(conn_info.username(), pass).map_err(|e| {
            log::error!("IMAP login failed: {:?}", e);
            e.0
        })?;
        imap_session
    } else {
        bail!("Only imaps is supported")
    };

    let mailbox_name = if target.len() == 0 {
        IMAP_INBOX_PREFIX.to_string()
    } else {
        format!("{}.{}", IMAP_INBOX_PREFIX, target)
    };

    // we want to fetch the first email in the INBOX mailbox
    let mut select = imap_session.select(&mailbox_name);
    if let Err(_err) = &select {
        // could not select mailbox, try to create it
        log::info!("Creating target folder:  {}", &mailbox_name);

        let create_result = imap_session.create(&mailbox_name);
        if let Err(err) = create_result {
            log::error!("Can't create target folder: {} {}", &mailbox_name, err);
            return Err(anyhow!("Can't create target folder: {}", err));
        }
        select = imap_session.select(&mailbox_name);
    }
    if select.is_err() {
        log::error!("Creating select imap folder:  {}", &mailbox_name);
        bail!("Creating select imap folder:  {}", &mailbox_name)
    }

    let imap_flags = flags2imap(flags);

    let append = imap_session.append_with_flags(&mailbox_name, content, &imap_flags);

    if let Err(err) = append {
        log::error!("Can't append to target folder: {} {}", &mailbox_name, err);
        bail!("Can't append to target folder: {}", err);
    } else {
        log::info!("Message stored in mailbox: {}", &mailbox_name);
    }

    Ok(())
}

/// Checks Args for configured targets and stores mail there
async fn store_message(
    config: &Config,
    content: &str,
    target: &str,
    flags: &Vec<String>,
) -> Result<()> {
    if let Some(maildir) = &config.maildir_path {
        // wrap in async runner
        return store_to_maildir(maildir.as_path(), content, target, flags);
    }
    if let Some(imap_url) = &config.imap_url {
        // wrap in async runner
        return store_to_imap(imap_url, content, target, flags, config.insecure);
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = Args::parse();
    // Get config file
    let config_path = args.config_file.resolve();
    let config = if let Ok(f) = File::open(&config_path) {
        // Parse config with serde
        let mut reader = BufReader::new(f);
        let mut contents = String::new();
        let res = reader.read_to_string(&mut contents);
        if let Err(err) = res {
            log::error!("Error reading config file: {}", &err);
            return ExitCode::from(2);
        }

        match toml::from_str::<<Config as ClapSerde>::Opt>(&contents) {
            // merge config already parsed from clap
            Ok(config) => Config::from(config).merge(&mut args.config),
            Err(err) => panic!("Error in configuration file:\n{}", err),
        }
    } else {
        // If there is not config file return only config parsed from clap
        Config::from(&mut args.config)
    };
    // println!("Config: {:?}", &config);

    setup_logging(&config);
    let result = run(&config).await;

    if result.is_success() {
        log::info!("{}", result);
        ExitCode::SUCCESS
    } else {
        log::error!("{}", result);
        ExitCode::from(1)
    }
}

pub async fn run(config: &Config) -> ProcessResult {
    let mut rv = ProcessResult::default();

    let mut content: String = String::new();

    let file_name = &config.file;

    if file_name == "-" {
        let res = std::io::stdin().lock().read_to_string(&mut content);
        if res.is_err() {
            log::error!("Can't read stdin: {}", res.err().unwrap());
        }
    } else {
        let mut file = File::open(file_name).unwrap();
        let res = file.read_to_string(&mut content);
        if res.is_err() {
            log::error!("Can't read stdin: {}", res.err().unwrap());
        }
    }
    let parsed = parse_mail(content.as_bytes());

    let mut user: String = config.unknown_user.clone();
    let mut user_found = false;
    let mut has_errors = false;
    let mut path_name_context = tera::Context::new();

    match parsed {
        Ok(message) => {
            if let Some(overwrite_user) = &config.overwrite_user {
                user.clone_from(overwrite_user);
                user_found = true;
            } else if let Some(extracted_user) = extract_user(&message) {
                user = extracted_user;
                user_found = true;
            };
            let user_option = if user_found { Some(user.clone()) } else { None };
            rv.user = user_option.clone();
            let res = extract_files(&message, &config, &user_option).await;

            match &res {
                Ok((files, errors)) => {
                    path_name_context.insert("errors", errors);
                    path_name_context.insert("num_files", &files.len());
                    path_name_context.insert("files", &files);
                    log::info!(
                        "Found {} files for user {}. {} Errors",
                        files.len(),
                        &user,
                        errors
                    );
                    if errors > &0 {
                        has_errors = true;
                    }
                    rv.files = files.clone();
                    rv.num_errors = *errors;
                }
                Err(e) => {
                    log::error!("Error: {}", e);
                    has_errors = true;
                }
            };
        }
        Err(e) => {
            log::error!("Error, can't parse mime email: {}", e);
            has_errors = true;
        }
    };
    let _ = path_name_context.insert("has_errors", &has_errors);
    let _ = path_name_context.insert("user", &user);
    // calculate the output folder name
    let mut template = create_template_engine();
    let mail_template = &config.mail_template;
    let target_folder = template
        .render_str(&mail_template, &path_name_context)
        .unwrap_or_else(|err| {
            log::error!(
                "Can´t render output folder path: {}. Template was: '{}'",
                &err,
                &mail_template
            );
            log::error!("Fallback folder '{}'", &FALLBACK_MAIL_TARGET);
            FALLBACK_MAIL_TARGET.to_owned()
        });
    let flags = if has_errors {
        &config.error_flags
    } else {
        &config.success_flags
    };

    // backoff::
    let mut retry_backoff = backoff::ExponentialBackoff::default();
    loop {
        log::debug!("Store message");
        let store_result = store_message(&config, &content, &target_folder, &flags).await;
        match store_result {
            Ok(_x) => {
                break;
            }
            Err(e) => {
                let wait = retry_backoff.next_backoff();
                log::warn!("Error storing mail: {}", e);
                match wait {
                    Some(wait) => {
                        log::info!("Retry in: {} seconds", wait.as_secs());
                        tokio::time::sleep(wait).await
                    }
                    None => {
                        log::error!("Maximum number of retries reached.");
                        break;
                    }
                }
            }
        };
    }

    // pip message if requested
    if config.stdout {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();

        let res = handle.write_all(content.as_bytes());
        if res.is_err() {
            log::error!("Can't write to stdout: {}", res.err().unwrap());
            rv.num_errors += 1;
        }
    };
    rv
}

pub fn setup_logging(config: &Config) {
    // configure logging
    let logging = stderrlog::new()
        .module(module_path!())
        .quiet(config.quiet)
        .verbosity(config.verbose as usize)
        .timestamp(stderrlog::Timestamp::Second)
        .init();

    if let Err(err) = &logging {
        println!("Error setting up logging: {}", err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use walkdir;

    #[test]
    fn test_user_extraction() {
        macro_rules! test_user {
            ($m:expr, None) => {
                let msg1 = parse_mail($m.as_ref()).unwrap();
                assert_eq!(extract_user(&msg1), None);
            };
            ($m:expr, $exp:expr) => {
                let msg1 = parse_mail($m.as_ref()).unwrap();
                assert_eq!(extract_user(&msg1), Some($exp.to_string()));
            };
        }
        test_user!(
            "From: test@example.com\n\
            To: office@example.com\n\
            ",
            "test"
        );
        test_user!(
            "\
            From: test@test.com\n\
            To: office@example.com\n\n\
            ",
            None
        );
        test_user!(
            "\
            From: test1@test.com\n\
            To: office+user1@example.com\n\n\
            ",
            "user1"
        );
        test_user!(
            "\
            From: foo+user1@example.com\n\
            To: office@example.com\n\n\
            ",
            "foo"
        );
    }
    #[test]
    fn test_escape_fn() {
        let mut tt = create_template_engine();
        let mut context = tera::Context::new();
        context.insert("file_name", "sH\\itty/fIl name.xml");
        assert_eq!(
            tt.render_str("{{ file_name | escape_filename}}", &context)
                .unwrap(),
            "sH__itty__fIl name.xml".to_owned()
        );
    }

    #[test]
    fn test_flags() {
        let flag_list = vec!["\\Flagged".to_owned(), "myflag".to_owned()];
        assert_eq!(
            flags2imap(&flag_list),
            vec![Flag::Flagged, Flag::Custom("myflag".into())]
        );

        let flags2 = vec!["\\Flagged".to_owned(), "m".to_owned()];
        assert_eq!(flags2maildir(&flags2), "Fm".to_owned());
    }

    #[tokio::test]
    async fn test_local_integration() {
        let dir = std::env::temp_dir();
        let files_path = dir.join("files");
        let maildir_path = dir.join("maildir");
        let mut config = Config {
            file: "test-data/test_email1.eml".to_owned(),
            local_path: Some(files_path.clone()),
            verbose: 3,
            output_template: DEFAULT_OUTPUT_TEMPLATE.into(),
            mail_template: DEFAULT_MAIL_TEMPLATE.into(),
            maildir_path: Some(maildir_path.clone()),
            ..Config::default()
        };
        setup_logging(&config);
        let res1 = run(&config).await;
        assert_eq!(res1.num_errors, 0);
        assert_eq!(res1.files, vec!["test1/sample1.pdf".to_owned()]);
        assert_eq!(res1.user, Some("test1".to_owned()));
        assert!(files_path.exists());
        let out_path = files_path.join("test1/sample1.pdf");
        assert!(out_path.exists());
        assert_eq!(std::fs::remove_file(&out_path).unwrap(), ());
        // check if mail was delivered correctly into the proper maildir folder
        let checkmail = tokio::task::spawn_blocking(move || {
            let mut found = 0;
            for entry in walkdir::WalkDir::new(maildir_path)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let f_name = entry.file_name().to_string_lossy();
                if entry.file_type().is_file()
                    && f_name.contains(":2,")
                    && entry.path().to_string_lossy().contains("test1.done")
                {
                    found += 1;
                }
            }
            found
        })
        .await;
        assert_eq!(checkmail.unwrap(), 1);

        config.file = "test-data/test_email_no_plus.eml".to_owned();
        let res2 = run(&config).await;
        assert_eq!(res2.num_errors, 0);
        assert_eq!(res2.user, Some("user1".to_owned()));
        assert_eq!(res2.files, vec!["user1/sample2.pdf".to_owned()]);
        let out_path = files_path.join("user1/sample2.pdf");
        assert!(out_path.exists());
        assert_eq!(std::fs::remove_file(&out_path).unwrap(), ());
    }

    #[tokio::test]
    #[ignore]
    async fn test_webdav_integration() {
        let target = std::env::var("TARGET").unwrap_or("localhost".into());
        let http_target = format!("http://test:testme@{}:4918/", target);
        let imap_target = format!("imaps://test:testme@{}/", target);
        let mut config = Config {
            file: "test-data/test_email1.eml".to_owned(),
            http_path: Some(http_target.clone()),
            imap_url: Some(imap_target.clone()),
            verbose: 3,
            insecure: true,
            output_template: DEFAULT_OUTPUT_TEMPLATE.into(),
            mail_template: DEFAULT_MAIL_TEMPLATE.into(),
            ..Config::default()
        };
        setup_logging(&config);
        log::info!(
            "Using TARGET={}, use environment variable to change",
            target
        );
        let res1 = run(&config).await;
        assert_eq!(res1.num_errors, 0);
        assert_eq!(res1.files, vec!["test1/sample1.pdf".to_owned()]);
        assert_eq!(res1.user, Some("test1".to_owned()));

        // check if the file was created at the webdav server
        let check_url = format!("{}/{}", &http_target, "test1/sample1.pdf");
        let req = reqwest::get(&check_url).await;
        assert!(req.is_ok());
    }
}

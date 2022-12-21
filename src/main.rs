// copyright: B1 Systems GmbH <info@b1-systems.de>, 2022
// license: GPLv3+, http://www.gnu.org/licenses/gpl-3.0.html
// author: Daniel Poelzleithner <poelzleithner@b1-systems.de>, 2022

/// Parses the email from file or stdin. Extracts all pdf attachments 
/// and uploads the file to a cloud storage or local path

extern crate base64;
extern crate mailparse;

#[macro_use]
extern crate log;

use clap::{command, arg, value_parser, ArgMatches};
use mailparse::*;
use std::cell::RefCell;
use std::fmt::Display;
use std::str::FromStr;
use std::{fs::File, cell::Cell};
use std::path::PathBuf;
use std::string::*;
use std::io::prelude::*;
use anyhow::{Result};
use tokio;

// lazy_static! {
//     static ref DEFAULT_EXTRACT_MIME: MimeArguments = MimeArguments(vec![
//         "application/pdf".to_string(),
//     ]);
// }

const DEFAULT_EXTRACT_MIMES: [&'static str; 1] = [
    "application/pdf"
];
const UNKNOWN_USER_DEFAULT: &'static str = "_UNKNOWN";
const ACCEPTED_MIMETYPES: &'static str = "accepted_mimetypes";

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
// #[derive(Parser, Debug)]
// #[command(author, version, about, long_about = None)]
// struct Args {
//    /// Name of the person to greet
//    #[arg(short, long)]
//    name: String,

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
// }

async fn extract_files(content: &str, args: &ArgMatches) -> Result<(Option<String>, Vec<String>, u32)> {
    let parsed = parse_mail(content.as_bytes()).unwrap();

    let mut files = Vec::new();
    let mut user: Option<String> = None;
    let mut errors = 0;
    
    let mut unknown = 0;

    let output = create_object_store(args)?;

    // parse the user to which this email belongs to
    // It extracts from the "To: something+user@example.com" the user part
    if let Some(to) = parsed.headers.get_first_value("to") {
        if let Ok(parsed_addr) = mailparse::addrparse(&to) {
            if parsed_addr.len() > 0 {
                match &parsed_addr[0] {
                    MailAddr::Single(info) => {
                        let v: Vec<&str> = info.addr.split_terminator('+').collect();
                        if v.len() == 2 {
                            // substring before @
                            let only_name: Vec<&str> = v[1].split_terminator('@').collect();
                            if only_name.len() == 2 {
                                user = Some(only_name[0].to_string());
                            }
                        }
                    },
                    _ => unimplemented!()
                }
            }
        }
    }

    for subpart in parsed.subparts.iter() {
        let mimes = args.get_one::<MimeArguments>(ACCEPTED_MIMETYPES).unwrap();
        if mimes.0.contains(&subpart.ctype.mimetype) {
            let dispos = &subpart.get_content_disposition();
            if dispos.disposition == DispositionType::Attachment {
                println!("{:?}", dispos.params);
                let filename: String = dispos.params
                    .get("filename")
                        .map(|x| x.clone())
                        .unwrap_or_else(||{
                            unknown += 1;
                            format!("attachment-{}", unknown)});
                // output filename
                let path = format!("{}/{}",
                    user.as_ref().unwrap_or(&args.get_one("unknown_user").unwrap()),
                    &filename);
                
                // write to backend store
                log::info!("Save file: {}", &path);
                let body = subpart.get_body_raw();
                if let Ok(body_vec) = body {
                    let success = output.put(&path.clone().into(), body_vec.into()).await;
                    files.push(path);
                } else {
                    log::warn!("Can't get body of attachment: {}", body.err().unwrap());
                    errors += 1;
                }

            }
        }
    }
    
    Ok((user, files, errors))
}

/// Creates the object_store to save objects to.
fn create_object_store(args: &ArgMatches) -> Result<Box<dyn object_store::ObjectStore>> {
    if let Some(lpath) = args.get_one::<PathBuf>("local_path") {
        return Ok(Box::new(object_store::local::LocalFileSystem::new_with_prefix(lpath)?));
    }
    anyhow::bail!("Please specify storage backend");
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    //let args = Args::parse();
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

    let mut extract_mimes = MimeArguments(DEFAULT_EXTRACT_MIMES.iter().map(|v| v.to_string()).collect::<Vec<String>>());

    let matches = command!() // requires `cargo` feature
        .arg(
            arg!(--unknown_user <NAME> "Name if user can't be identified")
            .default_value(UNKNOWN_USER_DEFAULT)
            .required(false),
        )
        .arg(
            arg!(
                --local_path <PATH> "Name if user can't be identified"
            )
            .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            arg!(
                --accepted_mimetypes <ACCEPTED_MIMETYPES> "List of mimetypes to accept"
            )
            .required(false)
            .default_value(extract_mimes)
            .value_parser(MimeArguments::from_str),
        )
        .arg(
            arg!(
                [FILE] "File to parse, - for stdin"
            )
            .required(false)
            .default_value("-"),
        )
        .arg(arg!(
            -d --debug ... "Turn debugging information on"
            )
            .action(clap::ArgAction::SetTrue))
        .get_matches();


    // let cfg = App::new("prog")
    //         .arg(Arg::with_name("unknown_user")
    //                 .long("unknown_user")
    //                 .takes_value(true)
    //                 .value_name("NAME")
    //                 .help("Name when user can't be determined"));

    let mut content: String = String::new();

    if matches.get_flag("debug") {
        log::set_max_level(log::LevelFilter::Debug);
    } else {
        log::set_max_level(log::LevelFilter::Info);
    }

    let file_name = matches.get_one::<String>("FILE").unwrap();
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

    let res = extract_files(&content, &matches).await;
    let mut move_failed = false;
    match &res {
        Ok((user, files, errors)) => {
            log::info!("Found {} files for user {}", files.len(), user.clone().unwrap_or("unknown".into()));
            if errors > &0 {
                move_failed = true;
            }
        }
        Err(e) => {
            log::error!("Error: {}", e);
        }
    };

    // copy message to imap
}
//! `orchestrator://open?file=…&line=…`, the app's own deep link.
//!
//! The same shape as `phpstorm://open`, so a tool that can already point an editor
//! at a file can point this app at one. The file opens in the file pane of
//! whichever open checkout holds it.
//!
//! **Two ways in, one per platform.** macOS hands a URL to the running app as an
//! Apple Event. Linux starts the binary again with the URL as an argument, and that
//! second process cannot open a window — the host's port and lock are taken — so
//! it hands the URL to the running host over HTTP, on [`crate::host::PORT`] and the
//! token in [`TOKEN_FILE`], and exits. When nothing is running it is the app, and
//! the host takes the link once it is up.

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::path::PathBuf;

/// The scheme, without `://`.
pub const SCHEME: &str = "orchestrator";

/// Where the running app leaves its host token for a second process to read.
///
/// **In the config dir, readable by its owner only.** The token is what a
/// same-user process could already reach — it is in the running app's memory and
/// in `ps` for every child it spawns — so this file costs nothing the daemon's own
/// threat model does not already concede (README, "the token").
pub const TOKEN_FILE: &str = "host.token";

/// A file to open, as a link names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OpenFile {
    /// Absolute, as the link gave it. Resolved by the host, which is where every
    /// other path meets the checkouts it is compared with.
    pub path: String,
    /// 1-based, or 0 for none.
    pub line: u32,
}

/// Read `orchestrator://open?file=/abs/path&line=42`.
///
/// `file` is required and must be absolute: a link comes from outside any
/// checkout, so a relative path has nothing to be relative to. `line` is optional,
/// and anything that is not a number is no line rather than a refusal — an editor
/// link carries `column` and friends, and those are ignored the same way.
pub fn parse(url: &str) -> Result<OpenFile> {
    let rest = url
        .strip_prefix(SCHEME)
        .and_then(|r| r.strip_prefix(':'))
        .with_context(|| format!("not an {SCHEME}: link"))?;
    let rest = rest.trim_start_matches('/');
    let Some(query) = rest.strip_prefix("open?") else {
        bail!("only {SCHEME}://open?file=… is understood");
    };
    let (mut file, mut line) = (None, 0);
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "file" => file = Some(decode(v)?),
            "line" => line = decode(v)?.parse().unwrap_or(0),
            _ => {}
        }
    }
    let path = file
        .filter(|f| !f.is_empty())
        .context("the link names no file")?;
    if !std::path::Path::new(&path).is_absolute() {
        bail!("the link's file must be an absolute path");
    }
    Ok(OpenFile { path, line })
}

/// [`parse`], and the file resolved on disk.
///
/// Resolved because the checkout paths it is matched against are, and a link
/// through a symlinked home would otherwise match no checkout at all. Blocking:
/// it touches the filesystem, so an async caller runs it off the runtime.
pub fn resolve(url: &str) -> Result<OpenFile> {
    let mut f = parse(url)?;
    let real =
        std::fs::canonicalize(&f.path).with_context(|| format!("{} is not there", f.path))?;
    if !real.is_file() {
        bail!("{} is not a file", f.path);
    }
    f.path = real.to_string_lossy().into_owned();
    Ok(f)
}

/// Percent-decoding, with `+` as a space the way a query string spells one.
fn decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'%' => {
                let hex = s.get(i + 1..i + 3).context("a truncated %-escape")?;
                out.push(u8::from_str_radix(hex, 16).context("a bad %-escape")?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).context("the link is not UTF-8")
}

/// The first argument that is one of this app's links.
pub fn in_args(args: impl IntoIterator<Item = String>) -> Option<String> {
    args.into_iter()
        .skip(1)
        .find(|a| a.starts_with(&format!("{SCHEME}:")))
}

fn token_path() -> Result<PathBuf> {
    Ok(orchd::config::Config::config_dir()?.join(TOKEN_FILE))
}

/// Leave the host token where a second launch can find it.
pub fn write_token(token: &str) -> Result<()> {
    let path = token_path()?;
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(token.as_bytes())?;
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

/// Hand `url` to the host running on `port`, and say whether it took it.
///
/// Plain HTTP over a loopback socket rather than a client crate: one POST, to a
/// server this binary also wrote, is a few lines. Short timeouts, because the
/// person clicked a link and a host that is not answering is a host that is not
/// there — the caller then starts the app itself.
pub fn forward(port: u16, url: &str) -> Result<()> {
    let token = std::fs::read_to_string(token_path()?).context("no running app left its token")?;
    send(port, token.trim(), url)
}

pub(crate) fn send(port: u16, token: &str, url: &str) -> Result<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = std::time::Duration::from_secs(2);
    let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("nothing is listening on port {port}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let body = serde_json::json!({ "url": url }).to_string();
    write!(
        stream,
        "POST /api/host/open HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-orch-token: {token}\r\n\
         content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    let status = answer.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        bail!(
            "the running app refused the link: {}",
            answer.lines().last().unwrap_or("")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_names_a_file_and_maybe_a_line() {
        let got = parse("orchestrator://open?file=/home/me/src/app.rs&line=42").unwrap();
        assert_eq!(
            got,
            OpenFile {
                path: "/home/me/src/app.rs".into(),
                line: 42
            }
        );
        // No line, a line that is not a number, and the fields an editor link adds.
        assert_eq!(parse("orchestrator://open?file=/a").unwrap().line, 0);
        assert_eq!(parse("orchestrator://open?file=/a&line=x").unwrap().line, 0);
        assert_eq!(
            parse("orchestrator://open?line=3&column=9&file=/a").unwrap(),
            OpenFile {
                path: "/a".into(),
                line: 3
            }
        );
        // The single-slash spelling some tools produce.
        assert_eq!(parse("orchestrator:/open?file=/a").unwrap().path, "/a");
    }

    #[test]
    fn a_path_is_percent_decoded() {
        assert_eq!(
            parse("orchestrator://open?file=/Users/me/My%20Code/a+b.md")
                .unwrap()
                .path,
            "/Users/me/My Code/a b.md"
        );
        assert_eq!(
            parse("orchestrator://open?file=%2Fhome%2Fme%2F%C3%A9t%C3%A9.txt")
                .unwrap()
                .path,
            "/home/me/été.txt"
        );
        assert!(
            parse("orchestrator://open?file=/a%2").is_err(),
            "a truncated escape"
        );
    }

    #[test]
    fn what_is_not_a_file_link_is_refused() {
        for bad in [
            "phpstorm://open?file=/a",
            "orchestrator://close?file=/a",
            "orchestrator://open?line=3",
            "orchestrator://open?file=relative/path",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_a_link_argument_is_a_link() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            in_args(args(&["orchestrator", "orchestrator://open?file=/a"])).as_deref(),
            Some("orchestrator://open?file=/a")
        );
        assert_eq!(
            in_args(args(&["orchestrator", "--install-desktop-entry"])),
            None
        );
        // argv[0] is the binary, whatever it is called.
        assert_eq!(in_args(args(&["orchestrator:weird-name"])), None);
    }
}

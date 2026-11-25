//! Updater against a local fake GitHub (wiremock): release lookup with
//! ETag / rate limits, and a full self-update into a temporary
//! `install.sh` layout, including the corrupt-download rollback.

use std::io::Write;
use std::path::Path;

use ferrotune::update::install::{self, Stage};
use ferrotune::update::method::Layout;
use ferrotune::update::release::{self, CheckOutcome};
use ferrotune::update::{Endpoints, UpdateError};
use sha2::{Digest, Sha256};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const VERSION: &str = "99.0.0";
const APP_ID: &str = "dev.ferro.ferrotune";

fn tar_entry(name: &str, kind: u8, data: &[u8]) -> Vec<u8> {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000755\0");
    h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = kind;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut v = h.to_vec();
    v.extend_from_slice(data);
    v.resize(v.len().div_ceil(512) * 512, 0);
    v
}

/// The tarball `release.yml` builds, with a dummy binary.
fn release_tarball() -> Vec<u8> {
    let top = format!("ferrotune-v{VERSION}-linux-x86_64");
    let mut tar = Vec::new();
    for (name, kind, data) in [
        (format!("{top}/"), b'5', &b""[..]),
        (format!("{top}/ferrotune"), b'0', b"#!/bin/sh\necho new\n"),
        (format!("{top}/README.md"), b'0', b"readme"),
        (format!("{top}/packaging/"), b'5', b""),
        (
            format!("{top}/packaging/{APP_ID}.desktop"),
            b'0',
            b"[Desktop Entry]\nName=Ferrotune\nExec=ferrotune\nIcon=dev.ferro.ferrotune\n",
        ),
        (format!("{top}/packaging/{APP_ID}.svg"), b'0', b"<svg new/>"),
    ] {
        tar.extend(tar_entry(&name, kind, data));
    }
    tar.extend_from_slice(&[0u8; 1024]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn release_json(base: &str) -> serde_json::Value {
    let name = format!("ferrotune-v{VERSION}-linux-x86_64.tar.gz");
    serde_json::json!({
        "tag_name": format!("v{VERSION}"),
        "draft": false,
        "prerelease": false,
        "html_url": format!("https://github.com/FerroApps/Ferrotune/releases/tag/v{VERSION}"),
        "assets": [
            {"name": name.clone(), "browser_download_url": format!("{base}/dl/{name}"), "size": 100},
            {"name": format!("{name}.sha256"), "browser_download_url": format!("{base}/dl/{name}.sha256"), "size": 90}
        ]
    })
}

async fn fake_github(tarball: Vec<u8>, sum_of: &[u8]) -> MockServer {
    let server = MockServer::start().await;
    let name = format!("ferrotune-v{VERSION}-linux-x86_64.tar.gz");
    Mock::given(method("GET"))
        .and(path("/repos/FerroApps/Ferrotune/releases/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_json(release_json(&server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/dl/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/dl/{name}.sha256")))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{}  {name}\n", sha_hex(sum_of))),
        )
        .mount(&server)
        .await;
    server
}

fn endpoints(server: &MockServer) -> Endpoints {
    Endpoints::resolve(Some(&format!(
        "{}/repos/FerroApps/Ferrotune/releases/latest",
        server.uri()
    )))
}

/// A home with an existing `install.sh` install of version 0.1.0.
fn old_install(root: &Path) -> Layout {
    let layout = Layout::from_env(root, Some(&root.join("bin")), Some(&root.join("data")));
    std::fs::create_dir_all(&layout.bin_dir).unwrap();
    std::fs::write(layout.binary("ferrotune"), "old-binary").unwrap();
    std::fs::create_dir_all(layout.state_dir()).unwrap();
    std::fs::write(layout.version_file(), "0.1.0\n").unwrap();
    std::fs::create_dir_all(layout.icon().parent().unwrap()).unwrap();
    std::fs::write(layout.icon(), "<svg old/>").unwrap();
    layout
}

fn dir_names(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn release_lookup_uses_etag_and_backs_off() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/latest"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/latest"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_json(release_json(&server.uri())),
        )
        .mount(&server)
        .await;
    let ends = Endpoints::resolve(Some(&format!("{}/latest", server.uri())));
    let first = release::check(&ends, None, 0).await.unwrap();
    let CheckOutcome::Latest { release, etag } = first else {
        panic!("expected a release");
    };
    assert_eq!(release.unwrap().version, VERSION);
    assert_eq!(etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        release::check(&ends, Some("\"v1\""), 0).await.unwrap(),
        CheckOutcome::NotModified
    );
    // The request identifies the app and nothing else.
    let reqs = server.received_requests().await.unwrap();
    let ua = reqs[0].headers.get("user-agent").unwrap().to_str().unwrap();
    assert_eq!(ua, format!("ferrotune/{}", env!("CARGO_PKG_VERSION")));
    assert!(reqs[0].headers.get("authorization").is_none());
    assert!(reqs[0].headers.get("cookie").is_none());

    let limited = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&limited)
        .await;
    let ends = Endpoints::resolve(Some(&format!("{}/latest", limited.uri())));
    assert_eq!(
        release::check(&ends, None, 0).await,
        Err(UpdateError::RateLimited(120))
    );
}

#[tokio::test]
async fn self_update_replaces_the_install_sh_layout() {
    let tarball = release_tarball();
    let server = fake_github(tarball.clone(), &tarball).await;
    let ends = endpoints(&server);
    let tmp = tempfile::tempdir().unwrap();
    let layout = old_install(tmp.path());
    let old_inode = std::fs::metadata(layout.binary("ferrotune")).unwrap();

    let CheckOutcome::Latest {
        release: Some(rel), ..
    } = release::check(&ends, None, 0).await.unwrap()
    else {
        panic!("expected a release");
    };
    let stages = std::sync::Mutex::new(Vec::new());
    let installed = install::run(&ends, &rel, &layout, &|s| stages.lock().unwrap().push(s))
        .await
        .unwrap();
    assert_eq!(installed.version, VERSION);
    assert_eq!(installed.binary, layout.binary("ferrotune"));

    let bin = layout.binary("ferrotune");
    assert_eq!(
        std::fs::read_to_string(&bin).unwrap(),
        "#!/bin/sh\necho new\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(&bin).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        assert_ne!(
            meta.ino(),
            old_inode.ino(),
            "renamed over, not rewritten in place"
        );
    }
    let _ = old_inode;
    assert_eq!(
        std::fs::read_to_string(layout.version_file()).unwrap(),
        format!("{VERSION}\n")
    );
    let entry = std::fs::read_to_string(layout.desktop_entry()).unwrap();
    assert!(
        entry.contains(&format!("\nExec={}\n", bin.display())),
        "{entry}"
    );
    assert_eq!(
        std::fs::read_to_string(layout.icon()).unwrap(),
        "<svg new/>"
    );
    // No temp dir, staged or backup file left behind.
    assert_eq!(dir_names(&layout.bin_dir), vec!["ferrotune".to_owned()]);
    assert_eq!(
        dir_names(&layout.state_dir()),
        vec!["installed-version".to_owned()]
    );

    let stages = stages.into_inner().unwrap();
    assert!(matches!(stages.first(), Some(Stage::Downloading { .. })));
    assert!(stages.contains(&Stage::Verifying));
    assert_eq!(stages.last(), Some(&Stage::Installing));
}

#[tokio::test]
async fn corrupt_download_leaves_the_install_untouched() {
    let tarball = release_tarball();
    // The checksum file describes a different (the "real") tarball.
    let mut corrupt = tarball.clone();
    let n = corrupt.len();
    corrupt[n / 2] ^= 0xff;
    let server = fake_github(corrupt, &tarball).await;
    let ends = endpoints(&server);
    let tmp = tempfile::tempdir().unwrap();
    let layout = old_install(tmp.path());
    let CheckOutcome::Latest {
        release: Some(rel), ..
    } = release::check(&ends, None, 0).await.unwrap()
    else {
        panic!("expected a release");
    };
    let r = install::run(&ends, &rel, &layout, &|_| {}).await;
    assert_eq!(r, Err(UpdateError::Checksum));
    assert_eq!(
        std::fs::read_to_string(layout.binary("ferrotune")).unwrap(),
        "old-binary"
    );
    assert_eq!(
        std::fs::read_to_string(layout.version_file()).unwrap(),
        "0.1.0\n"
    );
    assert_eq!(
        std::fs::read_to_string(layout.icon()).unwrap(),
        "<svg old/>"
    );
    assert!(!layout.desktop_entry().exists());
    assert_eq!(dir_names(&layout.bin_dir), vec!["ferrotune".to_owned()]);
}

#[tokio::test]
async fn assets_on_other_hosts_are_refused() {
    let tarball = release_tarball();
    let server = fake_github(tarball.clone(), &tarball).await;
    let ends = endpoints(&server);
    let tmp = tempfile::tempdir().unwrap();
    let layout = old_install(tmp.path());
    let CheckOutcome::Latest {
        release: Some(mut rel),
        ..
    } = release::check(&ends, None, 0).await.unwrap()
    else {
        panic!("expected a release");
    };
    for a in &mut rel.assets {
        a.url = a.url.replace("127.0.0.1", "example.org");
    }
    let r = install::run(&ends, &rel, &layout, &|_| {}).await;
    assert!(matches!(r, Err(UpdateError::BlockedUrl(_))), "{r:?}");
    assert_eq!(
        std::fs::read_to_string(layout.binary("ferrotune")).unwrap(),
        "old-binary"
    );
}

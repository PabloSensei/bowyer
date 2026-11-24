use std::io::{Read, Write};
use std::net::TcpListener;

use base64::Engine;

use super::*;

/// A fresh key pair and the minisign signature of `data`: (public key file text, signature
/// file text).
fn signed(data: &[u8]) -> (String, String) {
    signed_as(data, "test")
}

/// [`signed`] with the trusted comment `comment`.
fn signed_as(data: &[u8], comment: &str) -> (String, String) {
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let signature = minisign::sign(Some(&pair.pk), &pair.sk, data, Some(comment), None).unwrap();
    (pair.pk.to_box().unwrap().into_string(), signature.into_string())
}

/// What the packager writes as the trusted comment of `file` signed at `time`.
fn packager_comment(time: u64, file: &str) -> String {
    format!("timestamp:{time}{}file:{file}", char::from(9))
}

fn expected(version: &str, signed_after: u64) -> Expected {
    Expected { version: semver::Version::parse(version).unwrap(), signed_after }
}

fn b64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

fn manifest(version: &str) -> Manifest {
    serde_json::from_str(&format!(
        r#"{{"version":"{version}","notes":"Faster","platforms":{{"windows-x86_64":{{"url":"https://example.test/Bowyer_setup.exe","signature":"sig"}}}}}}"#
    ))
    .unwrap()
}

#[test]
fn only_a_newer_version_is_a_release() {
    assert_eq!(newer(&manifest("0.1.0"), "0.1.0", "windows-x86_64").unwrap(), None);
    assert_eq!(newer(&manifest("0.0.9"), "0.1.0", "windows-x86_64").unwrap(), None);
    assert_eq!(newer(&manifest("0.2.0-beta.1"), "0.2.0", "windows-x86_64").unwrap(), None, "a pre-release of a version out is older");
    let release = newer(&manifest("v0.2.0"), "0.1.0", "windows-x86_64").unwrap().unwrap();
    assert_eq!(release.version, semver::Version::new(0, 2, 0));
    assert_eq!(release.notes, "Faster");
    assert_eq!(release.package.as_ref().unwrap().format(), "nsis");
    let elsewhere = newer(&manifest("0.2.0"), "0.1.0", "linux-x86_64").unwrap().unwrap();
    assert_eq!(elsewhere.package, None, "no package for this platform: the window only tells");
    assert!(matches!(newer(&manifest("next"), "0.1.0", "windows-x86_64"), Err(Error::Feed(_))));
}

/// Only a web page over `https` from the feed is kept to be opened.
#[test]
fn only_a_web_page_is_kept_for_the_release() {
    let with_page = |page: &str| {
        let mut manifest = manifest("0.2.0");
        manifest.page = Some(page.into());
        newer(&manifest, "0.1.0", "windows-x86_64").unwrap().unwrap().page
    };
    assert_eq!(with_page("https://example.test/releases/0.2.0").as_deref(), Some("https://example.test/releases/0.2.0"));
    assert_eq!(with_page("HTTPS://example.test/r").as_deref(), Some("HTTPS://example.test/r"));
    for page in ["http://example.test/r", "file:///C:/Windows/System32/calc.exe", "https://", "https://x/a b", "javascript:alert(1)", "ms-settings:"] {
        assert_eq!(with_page(page), None, "{page}");
    }
    assert_eq!(with_page(r"\\attacker\share\x.exe"), None);
    assert_eq!(with_page(r"https://x\y"), None);
}

/// A package's signature has to name its own file, of the announced version, signed after
/// the running program was built: an old signed package is not a new release.
#[test]
fn an_old_signed_package_is_not_taken_for_a_new_one() {
    let url = "https://example.test/download/v0.1.6/bowyer_0.1.6_x64-setup.exe";
    let new = expected("0.1.6", 1_795_500_000);
    let check = |comment: &str| check_comment(comment, url, &new);
    check(&packager_comment(1_795_600_000, "bowyer_0.1.6_x64-setup.exe")).unwrap();
    let refused = [
        packager_comment(1_795_400_000, "bowyer_0.1.6_x64-setup.exe"),
        packager_comment(1_795_600_000, "bowyer_0.1.5_x64-setup.exe"),
        packager_comment(1_795_600_000, "other.exe"),
        "timestamp:1795600000".to_owned(),
        "file:bowyer_0.1.6_x64-setup.exe".to_owned(),
    ];
    for comment in refused {
        assert!(matches!(check(&comment), Err(Error::Signature(_))), "{comment}");
    }
    // A file without a version in its name (the macOS bundle) is told by its time alone.
    let bundle = "https://example.test/download/v0.1.6/Bowyer.app.tar.gz";
    check_comment(&packager_comment(1_795_600_000, "Bowyer.app.tar.gz"), bundle, &new).unwrap();
    assert!(check_comment(&packager_comment(1_795_400_000, "Bowyer.app.tar.gz"), bundle, &new).is_err());
    // A pre-release keeps its whole version in the name.
    let beta = "https://example.test/download/v0.2.0-beta.1/bowyer_0.2.0-beta.1_x64-setup.exe";
    check_comment(&packager_comment(1_795_600_000, "bowyer_0.2.0-beta.1_x64-setup.exe"), beta, &expected("0.2.0-beta.1", 1)).unwrap();
    assert!(check_comment(&packager_comment(1_795_600_000, "bowyer_0.2.0-beta.1_x64-setup.exe"), beta, &expected("0.2.0", 1)).is_err());
    // Without a build time, the time is not asked about.
    check_comment(&packager_comment(5, "bowyer_0.1.6_x64-setup.exe"), url, &expected("0.1.6", 0)).unwrap();
}

#[test]
fn signatures_hold_in_every_spelling_and_catch_changes() {
    let data = b"the installer";
    let (key, signature) = signed(data);
    verify(data, &signature, &key).unwrap();
    verify(data, &b64(&signature), &b64(&key)).unwrap();
    let key_line = key.lines().nth(1).unwrap();
    verify(data, &signature, key_line).unwrap();

    assert!(matches!(verify(b"the installer!", &signature, &key), Err(Error::Signature(_))), "a changed file");
    let (other_key, _) = signed(data);
    assert!(matches!(verify(data, &signature, &other_key), Err(Error::Signature(_))), "another key");
    assert!(matches!(verify(data, "garbage", &key), Err(Error::Signature(_))));
    assert!(matches!(verify(data, &signature, "garbage"), Err(Error::Signature(_))));
}

#[test]
fn only_secure_or_local_addresses_are_fetched() {
    assert!(allowed("https://example.test/latest.json"));
    assert!(allowed("http://127.0.0.1:8123/latest.json"));
    assert!(allowed("http://localhost/x"));
    assert!(allowed("http://[::1]:80/x"));
    assert!(!allowed("http://example.test/latest.json"));
    assert!(!allowed("http://127.0.0.1.example.test/x"));
    assert!(!allowed("http://127.0.0.1:1@example.test/x"));
    assert!(!allowed("http://localhost@example.test/x"));
    assert!(!allowed("http://127.0.0.1\\@example.test/x"));
    assert!(!allowed("file:///etc/passwd"));
    assert!(matches!(check("http://example.test/latest.json", "0.1.0"), Err(Error::Address(_))));
}

#[test]
fn package_names_stay_in_their_folder() {
    assert_eq!(file_name("https://h/r/Bowyer_0.2.0_x64-setup.exe?x=1"), "Bowyer_0.2.0_x64-setup.exe");
    assert_eq!(file_name("https://h/../..%2f.."), "2f..");
    assert_eq!(file_name("https://h/"), "bowyer-update");
}

#[test]
fn installations_are_told_apart() {
    let dir = std::env::temp_dir().join(format!("bowyer-install-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("bowyer.exe");
    std::fs::write(&exe, b"").unwrap();
    assert_eq!(install_kind_of(&exe, None), Install::Unmanaged);
    std::fs::write(dir.join("uninstall.exe"), b"").unwrap();
    let expected = if cfg!(windows) { Install::Nsis(dir.clone()) } else { Install::Unmanaged };
    assert_eq!(install_kind_of(&exe, None), expected);
    let image = dir.join("Bowyer.AppImage");
    std::fs::write(&image, b"").unwrap();
    assert_eq!(install_kind_of(&exe, Some(image.clone())), Install::AppImage(image.clone()));
    assert_eq!(Install::AppImage(image).download_dir(Path::new("/cache")), dir);
    assert_eq!(Install::Unmanaged.format(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Serves each of `responses` (status line, body) to one connection, in order.
fn serve(responses: Vec<(&'static str, Vec<u8>)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for (status, body) in responses {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    address
}

#[test]
fn a_feed_and_its_package_come_over_a_local_server() {
    let package = b"new installer".to_vec();
    let (key, signature) = signed_as(&package, &packager_comment(2_000, "Bowyer_9.0.0_setup.exe"));
    let nine = expected("9.0.0", 1_000);
    let platform = platform();
    let feed = |base: &str, signature: &str| {
        format!(r#"{{"version":"9.0.0","platforms":{{"{platform}":{{"url":"{base}/Bowyer_9.0.0_setup.exe","signature":"{}"}}}}}}"#, b64(signature))
    };
    // The packages come from one server and the feed that names them from another.
    let base = serve(vec![("200 OK", Vec::new()), ("200 OK", package.clone()), ("200 OK", package[..4].to_vec()), ("404 Not Found", Vec::new())]);
    let manifest_body = feed(&base, &signature);
    let feed_url = serve(vec![("200 OK", manifest_body.into_bytes())]);

    let release = check(&format!("{feed_url}/latest.json"), "0.1.0").unwrap().unwrap();
    assert_eq!(release.version.major, 9);
    let package_entry = release.package.unwrap();
    let dir = std::env::temp_dir().join(format!("bowyer-update-{}", std::process::id()));

    // The first answer is empty: its signature cannot match, and nothing stays behind.
    assert!(matches!(download(&package_entry, &dir, &key, &nine), Err(Error::Signature(_))));
    assert!(!dir.join("Bowyer_9.0.0_setup.exe").exists());
    let path = download(&package_entry, &dir, &key, &nine).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), package);
    // A cut file fails its signature too.
    assert!(matches!(download(&package_entry, &dir, &key, &nine), Err(Error::Signature(_))));
    assert!(matches!(download(&package_entry, &dir, &key, &nine), Err(Error::Transfer(..))));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole way over a real NSIS installation: the feed, the package, its signature and the
/// installer in passive mode, which closes the running copy and starts the new one. Installs
/// into the folder of a test installation; run with `--ignored` and
/// `BOWYER_TEST_FEED=<url> BOWYER_TEST_KEY=<public key file> BOWYER_TEST_INSTALL=<folder>`.
#[test]
#[ignore]
fn an_installation_updates_itself() {
    let feed = std::env::var("BOWYER_TEST_FEED").expect("BOWYER_TEST_FEED");
    let key = std::fs::read_to_string(std::env::var("BOWYER_TEST_KEY").expect("BOWYER_TEST_KEY")).unwrap();
    let folder = PathBuf::from(std::env::var("BOWYER_TEST_INSTALL").expect("BOWYER_TEST_INSTALL"));
    let install = install_kind_of(&folder.join("bowyer.exe"), None);
    assert_eq!(install, Install::Nsis(folder.clone()));
    let release = check(&feed, "0.1.0").unwrap().expect("a newer version");
    let package = release.package.expect("a package for this platform");
    assert_eq!(install.format(), Some(package.format()));
    let dir = std::env::temp_dir().join(format!("bowyer-update-e2e-{}", std::process::id()));
    let expected = Expected { version: release.version.clone(), signed_after: 0 };
    match download(&package, &dir, &key, &expected) {
        Ok(path) => {
            apply(&install, &path).unwrap();
            eprintln!("installer started: {}", path.display());
        }
        Err(error) => panic!("{error}"),
    }
}

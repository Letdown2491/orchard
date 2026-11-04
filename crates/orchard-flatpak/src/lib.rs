//! The *only* place Orchard invokes the flatpak CLI. Keeping one seam means:
//!
//! - when Orchard itself runs inside a flatpak sandbox, every call is
//!   transparently wrapped in `flatpak-spawn --host` (the pattern Warehouse
//!   uses), because a sandboxed app cannot touch the host flatpak directly;
//! - commands are returned as [`Command`] builders so they are unit-testable
//!   without executing anything and the UI layer can add progress handling.
//!
//! Scope: the reviewed channel (Flathub, plus other enabled classic
//! remotes) installs to the installation that hosts its remote, the
//! **system** install in the common case (that is where flathub's
//! remotes and runtimes live). A remote added only to the **user**
//! installation is invisible to the system one, so its installs and
//! queries take `--user`; [`RemoteListing::system`] is the discriminator.
//! The publisher channel installs to the **user** installation: flatpak's
//! system-scope image install is broken as of 1.18.4 (see
//! [`install_image`]), and a user-scope install also needs no polkit
//! dialog, so no password on install.
//!
//! Install transport for Orchard-channel apps: `flatpak install` accepts
//! image locations directly (`oci:<dir>`, `oci-archive:<file>`,
//! `docker://<registry>/<repo>:<tag>`), auto-detected by prefix (verified
//! against flatpak 1.18.4, `app/flatpak-builtins-install.c`). The image's
//! config labels must carry `org.flatpak.ref` and `org.flatpak.metadata`;
//! flatpak reconstructs the ostree commit from the layers plus labels at
//! install time, so no ostree machinery is needed on the publish side.

use std::path::Path;
use std::process::Command;

/// Name of the flatpak remote the local index shim backs (future native
/// remote flow; the MVP installs images directly instead).
pub const REMOTE_NAME: &str = "orchard";

/// True when Orchard itself runs inside a flatpak sandbox.
pub fn running_in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Base `flatpak` invocation, escaping the sandbox if necessary.
pub fn flatpak_command() -> Command {
    if running_in_flatpak() {
        let mut cmd = Command::new("flatpak-spawn");
        cmd.arg("--host").arg("flatpak");
        cmd
    } else {
        Command::new("flatpak")
    }
}

/// Register the local index shim (`orchard-indexd`) as an OCI remote.
///
/// `index_url` looks like `oci+http://127.0.0.1:7780`. Note (validated in the
/// M0 spike, flatpak sources `common/flatpak-dir.c`): an `oci+http(s)` remote
/// fetches an **ostree summary** from the URL base, whose ref entries map to
/// image manifest digests, with `xa.oci-registry-uri` in the summary
/// metadata naming the registry that serves blobs. Serving that summary is
/// the future native-remote flow; until then the shim's HTTP catalog API is
/// what the client uses.
pub fn ensure_oci_remote(index_url: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["remote-add", "--oci", "--user", REMOTE_NAME, index_url]);
    cmd
}

/// Install a ref from a classic remote, e.g.
/// `flatpak install --system -y flathub org.gnome.Polari` or a
/// runtime branch (`runtime/org.freedesktop.Platform/x86_64/26.08`).
/// The scope must be one hosting the remote: a user-install-only
/// remote is unknown to the system installation and vice versa.
pub fn install_ref(ref_: &str, remote: &str, scope: Scope) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["install", scope.flag(), "-y", remote, ref_]);
    cmd
}

/// Whether a ref is deployed in a scope: the exit status of
/// `flatpak info <scope> <ref>`. Deployments do not cross scopes for
/// install-time dependency checks, so each scope asks for itself.
pub fn info_ref(scope: Scope, ref_: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["info", scope.flag(), ref_]);
    cmd
}

/// The flatpak installation an op targets. `System` is the machine-wide
/// install (polkit-gated); `User` is per-user under `$HOME`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

impl Scope {
    fn flag(self) -> &'static str {
        match self {
            Scope::System => "--system",
            Scope::User => "--user",
        }
    }
}

/// Install an OCI image directly, no remote involved (flatpak >= 1.17).
///
/// `location` is an image location as understood by `flatpak install`:
/// `oci:<dir>`, `oci-archive:<file>`, or `docker://<registry>/<repo>:<tag>`.
///
/// This is user scope, on purpose. Flatpak's image-install flow creates the
/// transient origin remote with an empty URL, and every system-helper trust
/// check keys on the URL (`flatpak_dir_get_remote_oci` wants an `oci+`
/// prefix), so `install --system` of any image location dies with "Can't
/// pull from untrusted non-gpg verified remote" on flatpak 1.18.4 through
/// current main. The user scope never consults the system helper: the pull
/// rides the image source directly, and that path works end to end
/// (verified against 1.18.4 with a local registry). A user-scope install
/// also needs no polkit dialog.
///
/// `no_deps` adds `--no-deps`: the image path checks the runtime
/// only against the installing scope's deployments, so when the
/// runtime deploys in the other scope (a user install running a
/// system runtime, the way Orchard itself is deployed), the check
/// must be skipped; flatpak resolves the runtime across
/// installations at run time.
pub fn install_image(location: &str, no_deps: bool) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["install", "--user", "-y"]);
    if no_deps {
        cmd.arg("--no-deps");
    }
    cmd.arg(location);
    cmd
}

/// Install a `.flatpak` bundle file, user scope. Unlike the `oci:`
/// image path, a bundle install updates in place when the ref is
/// already installed (verified: 0.64.13 bundle over 0.64.13), and
/// flatpak itself resolves missing runtimes from the enabled remotes.
pub fn install_bundle(path: &std::path::Path) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["install", "--user", "-y", "--bundle"]);
    cmd.arg(path);
    cmd
}

/// What a bundle file's header carries. The format is flatpak's
/// single-file artifact: a `flatpak\0` magic, a version and size
/// header, then the deploy ref and the app's full metadata ini
/// inline, before the ostree payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleInfo {
    /// The deploy ref, `app/<id>/<arch>/<branch>`.
    pub flatpak_ref: String,
    /// The metadata ini the deployed app would carry (the same
    /// source the permissions viewer and the runtime pre-flight read).
    pub metadata: String,
}

/// Read a bundle file's header. Only the first bytes move: the ini
/// starts at the `[Application]` section marker and ends at the
/// header's double-NUL, long before the payload begins.
pub fn bundle_metadata(path: &std::path::Path) -> std::io::Result<BundleInfo> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 64 * 1024];
    let read = file.read(&mut head)?;
    let head = &head[..read];
    if !head.starts_with(b"flatpak\0") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a flatpak bundle", path.display()),
        ));
    }
    let start = head
        .windows(13)
        .position(|w| w == b"[Application]")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} carries no [Application] section", path.display()),
            )
        })?;
    let end = head[start..]
        .windows(2)
        .position(|w| w == b"\0\0")
        .map(|p| start + p)
        .unwrap_or(read);
    Ok(BundleInfo {
        flatpak_ref: header_ref(head),
        metadata: String::from_utf8_lossy(&head[start..end]).into_owned(),
    })
}

/// The deploy ref rides the header before the metadata: a
/// length-prefixed string after the magic and sizes. Scan for the
/// `app/` (or `runtime/`) prefix and read to the first NUL, which is
/// tolerant of the exact header layout across bundle versions.
fn header_ref(head: &[u8]) -> String {
    for prefix in [b"app/".as_slice(), b"runtime/".as_slice()] {
        if let Some(start) = head.windows(prefix.len()).position(|w| w == prefix) {
            let rest = &head[start..];
            if let Some(nul) = rest.iter().position(|b| *b == 0) {
                return String::from_utf8_lossy(&rest[..nul]).into_owned();
            }
        }
    }
    String::new()
}

/// Update everything the `orchard` remote serves (plus whatever else is stale).
pub fn update() -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["update", "--system", "-y"]);
    cmd
}

/// Uninstall an app id from the given scope, at a branch when given:
/// a partial ref (`app.id//branch`) so parallel channel deployments
/// uninstall one at a time. User data is kept.
pub fn uninstall(scope: Scope, app_id: &str, branch: Option<&str>) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["uninstall", scope.flag(), "-y"]);
    cmd.arg(match branch {
        Some(branch) => format!("{app_id}//{branch}"),
        None => app_id.to_string(),
    });
    cmd
}

/// Launch an installed app, at a branch when given: parallel channel
/// deployments each get a launch of their own.
pub fn run(app_id: &str, branch: Option<&str>) -> Command {
    let mut cmd = flatpak_command();
    cmd.arg("run");
    cmd.arg(match branch {
        Some(branch) => format!("{app_id}//{branch}"),
        None => app_id.to_string(),
    });
    cmd
}

/// List installed apps across installations:
/// `flatpak list --app --columns=application,version,branch,origin,installation`.
/// Rows are tab-separated; `installation` is `user` or `system`.
pub fn list_installed() -> Command {
    let mut cmd = flatpak_command();
    cmd.args([
        "list",
        "--app",
        "--columns=application,version,branch,origin,installation",
    ]);
    cmd
}

/// A remote known to the host's flatpak, from `flatpak remotes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteListing {
    /// The remote's local name: the key `install` and `remote-info`
    /// route through.
    pub name: String,
    /// The remote's display title ("Flathub"). Empty when the remote
    /// ships none (flatpak prints `-` for an unset title).
    pub title: String,
    /// The remote's repository URL, empty when unknown. Subset
    /// remotes share their parent's URL.
    pub url: String,
    /// Raw options column, e.g. `system,filtered`, `user`,
    /// `no-enumerate`.
    pub options: String,
}

impl RemoteListing {
    fn has_option(&self, want: &str) -> bool {
        self.options.split(',').any(|opt| opt.trim() == want)
    }

    /// True when the remote was added with `--no-enumerate`: it asks
    /// stores not to list its contents. The origin remotes that
    /// `.flatpakref` installs create carry this flag.
    pub fn no_enumerate(&self) -> bool {
        self.has_option("no-enumerate")
    }

    /// True when the remote serves the system installation. A remote
    /// can exist in both installations; the system one is where the
    /// polkit-gated (passwordless-prompting) install flow already
    /// points, so it is preferred when present.
    pub fn system(&self) -> bool {
        self.has_option("system")
    }
}

/// The remotes known to the host:
/// `flatpak remotes --columns=name,title,url,options`. Disabled
/// remotes are omitted unless `--show-disabled` is passed, which this
/// deliberately does not.
pub fn list_remotes() -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["remotes", "--columns=name,title,url,options"]);
    cmd
}

/// The tab-separated rows `flatpak remotes` prints, one row per
/// remote per installation. An unset title or URL comes back empty.
pub fn parse_remotes(output: &str) -> Vec<RemoteListing> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let field = |text: Option<&str>| -> String {
                text.map(str::trim)
                    .filter(|text| !text.is_empty() && *text != "-")
                    .unwrap_or_default()
                    .to_string()
            };
            Some(RemoteListing {
                title: field(fields.next()),
                url: field(fields.next()),
                options: field(fields.next()),
                name,
            })
        })
        .collect()
}

/// Info about one installed app in the given scope: `flatpak info
/// <scope> <app>`. Its text output carries the installed size among
/// other fields.
pub fn info(scope: Scope, app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["info", scope.flag(), app_id]);
    cmd
}

/// The sandbox metadata ini of an app on a remote, before install:
/// `flatpak remote-info --show-metadata <scope> <remote> <app>`. This is
/// the manifest of the commit that would be installed.
pub fn remote_metadata(scope: Scope, remote: &str, app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["remote-info", "--show-metadata", scope.flag(), remote, app_id]);
    cmd
}

/// Update an installed app to its remote's current commit:
/// `flatpak update <scope> -y <app>`, at a branch when given. The
/// app's own origin remote supplies the new commit.
pub fn update_ref(scope: Scope, app_id: &str, branch: Option<&str>) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["update", scope.flag(), "-y"]);
    cmd.arg(match branch {
        Some(branch) => format!("{app_id}//{branch}"),
        None => app_id.to_string(),
    });
    cmd
}

/// Refresh a remote's AppStream catalog metadata:
/// `flatpak update --appstream <scope> <remote>`. This is the fetch
/// flatpak itself runs on its next update or search, pulled earlier
/// for a remote the user just added and never searched. Only catalog
/// metadata lands in flatpak's cache; nothing installed changes.
pub fn update_appstream(scope: Scope, remote: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["update", scope.flag(), "--appstream", remote]);
    cmd
}

/// Summary of the remote's current commit for an app, before install:
/// `flatpak remote-info <scope> <remote> <app>`: version and the
/// download / installed sizes of what would be installed.
pub fn remote_info(scope: Scope, remote: &str, app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["remote-info", scope.flag(), remote, app_id]);
    cmd
}

/// The download and installed sizes from `flatpak remote-info` output
/// (right-aligned keys, humanized values).
pub fn parse_remote_info(output: &str) -> (Option<String>, Option<String>) {
    let mut download = None;
    let mut installed = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Download Size:") {
            download = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Installed Size:") {
            installed = Some(rest.trim().to_string());
        }
    }
    (download, installed)
}

/// The sandbox metadata ini of an installed app, read straight from the
/// deploy (`app/<id>/current/active/metadata`). System install first,
/// then user. This is what powers the permissions view for apps that
/// came from a remote rather than an Orchard catalog.
pub fn installed_metadata(app_id: &str) -> Option<String> {
    let mut roots = vec![std::path::PathBuf::from("/var/lib/flatpak/app")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(
            std::path::PathBuf::from(home)
                .join(".local/share/flatpak/app"),
        );
    }
    roots
        .iter()
        .map(|root| root.join(app_id).join("current/active/metadata"))
        .find_map(|path| std::fs::read_to_string(path).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_strings(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// A synthetic bundle header: magic, junk header bytes, the deploy
    /// ref, then the metadata ini closed by the double-NUL and
    /// followed by payload bytes.
    #[test]
    fn bundle_metadata_reads_ref_and_ini() {
        let mut bytes = b"flatpak\0".to_vec();
        bytes.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // version + sizes
        bytes.extend_from_slice(b"\x13\0\0\0app/com.example.Zap/x86_64/stable\0");
        bytes.extend_from_slice(b"\x1e\0\0\0");
        let ini = "[Application]\nname=com.example.Zap\nruntime=org.freedesktop.Platform/x86_64/25.08\n";
        bytes.extend_from_slice(ini.as_bytes());
        bytes.extend_from_slice(b"\0\0payload-bytes-past-the-header");
        let path = std::env::temp_dir().join("orchard-bundle-header-test.flatpak");
        std::fs::write(&path, &bytes).unwrap();

        let info = bundle_metadata(&path).unwrap();
        assert_eq!(info.flatpak_ref, "app/com.example.Zap/x86_64/stable");
        assert_eq!(info.metadata, ini);
        assert!(!info.metadata.contains("payload"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn bundle_metadata_rejects_non_bundles() {
        let path = std::env::temp_dir().join("orchard-bundle-header-test.txt");
        std::fs::write(&path, b"not a bundle").unwrap();
        assert!(bundle_metadata(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn install_bundle_builds_expected_invocation() {
        let cmd = install_bundle(std::path::Path::new("/tmp/x.flatpak"));
        assert_eq!(
            arg_strings(&cmd),
            ["install", "--user", "-y", "--bundle", "/tmp/x.flatpak"]
        );
    }

    #[test]
    fn install_ref_builds_expected_invocation() {
        for (scope, flag) in [(Scope::System, "--system"), (Scope::User, "--user")] {
            let cmd = install_ref("org.gnome.Polari", "flathub", scope);
            let args = arg_strings(&cmd);
            let tail: Vec<String> = args.iter().skip(args.len() - 5).cloned().collect();
            assert_eq!(
                tail,
                ["install", flag, "-y", "flathub", "org.gnome.Polari"]
            );
        }
    }

    #[test]
    fn install_image_passes_location_through() {
        for location in [
            "oci:/abs/path/to/layout",
            "oci-archive:/tmp/x.oci.tgz",
            "docker://localhost:5000/hello:stable",
        ] {
            let cmd = install_image(location, false);
            let args = arg_strings(&cmd);
            // user scope: flatpak's system-scope image install is broken
            // (empty-URL origin remote), see install_image's doc
            assert!(args.contains(&"--user".to_string()));
            assert!(!args.contains(&"--no-deps".to_string()));
            assert!(args.last().is_some_and(|last| last == location));
        }
    }

    #[test]
    fn install_image_skips_the_dependency_check_on_demand() {
        let cmd = install_image("oci:/abs/path/to/layout", true);
        let args = arg_strings(&cmd);
        assert!(args.contains(&"--no-deps".to_string()));
        assert!(args.last().is_some_and(|last| last == "oci:/abs/path/to/layout"));
    }

    #[test]
    fn uninstall_targets_the_given_scope() {
        let cmd = uninstall(Scope::User, "rocks.orchard.Hello", None);
        assert_eq!(
            arg_strings(&cmd),
            ["uninstall", "--user", "-y", "rocks.orchard.Hello"]
        );
        let cmd = uninstall(Scope::System, "org.gnome.Polari", None);
        assert_eq!(
            arg_strings(&cmd),
            ["uninstall", "--system", "-y", "org.gnome.Polari"]
        );
        // a branch narrows the uninstall to that deployment alone
        let cmd = uninstall(Scope::User, "rocks.orchard.Hello", Some("beta"));
        assert_eq!(
            arg_strings(&cmd),
            ["uninstall", "--user", "-y", "rocks.orchard.Hello//beta"]
        );
    }

    #[test]
    fn remote_add_is_oci() {
        let cmd = ensure_oci_remote("oci+http://127.0.0.1:7780");
        let args = arg_strings(&cmd);
        assert!(args.contains(&"--oci".to_string()));
        assert!(args.contains(&"oci+http://127.0.0.1:7780".to_string()));
        assert!(args.contains(&REMOTE_NAME.to_string()));
    }

    #[test]
    fn update_ref_builds_expected_invocation() {
        for (scope, flag) in [(Scope::System, "--system"), (Scope::User, "--user")] {
            let cmd = update_ref(scope, "org.gnome.Polari", None);
            let args = arg_strings(&cmd);
            let tail: Vec<String> = args.iter().skip(args.len() - 4).cloned().collect();
            assert_eq!(tail, ["update", flag, "-y", "org.gnome.Polari"]);
        }
        let cmd = update_ref(Scope::User, "org.gnome.Polari", Some("beta"));
        assert!(arg_strings(&cmd).contains(&"org.gnome.Polari//beta".to_string()));
    }

    #[test]
    fn list_remotes_requests_the_four_columns() {
        let args = arg_strings(&list_remotes());
        assert!(args
            .contains(&"remotes".to_string()));
        assert!(args
            .contains(&"--columns=name,title,url,options".to_string()));
    }

    #[test]
    fn update_appstream_targets_the_scope_and_remote() {
        let cmd = update_appstream(Scope::User, "flatpark");
        let args = arg_strings(&cmd);
        let tail: Vec<String> = args.iter().skip(args.len() - 4).cloned().collect();
        assert_eq!(tail, ["update", "--user", "--appstream", "flatpark"]);
    }

    #[test]
    fn parse_remotes_reads_rows_and_unset_fields() {
        // The live shape: one row per remote per installation, tab
        // separated, `-` for an unset title.
        let listings = parse_remotes(
            "flathub\tFlathub\thttps://dl.flathub.org/repo/\tsystem,filtered\n\
             flathub\tFlathub\thttps://dl.flathub.org/repo/\tuser\n\
             onepassword-origin\t-\thttps://onepass.example/repo\tsystem,no-enumerate\n\
             viola\t-\thttps://violabrowser.com/downloads/flatpak/repo\tsystem\n",
        );
        assert_eq!(listings.len(), 4);
        assert_eq!(listings[0].name, "flathub");
        assert_eq!(listings[0].title, "Flathub");
        assert_eq!(listings[0].url, "https://dl.flathub.org/repo/");
        assert!(listings[0].system());
        assert!(!listings[0].no_enumerate());
        assert!(listings[2].no_enumerate());
        assert_eq!(listings[2].title, "", "a `-` title is unset");
        assert!(listings[3].system());
        assert_eq!(listings[3].title, "", "a `-` title is unset");
    }

    #[test]
    fn parse_remotes_skips_blank_rows() {
        assert!(parse_remotes("").is_empty());
        assert!(parse_remotes("\n").is_empty());
        // a name-less row is not a remote
        assert!(parse_remotes("\tTitle\thttps://x\tuser\n").is_empty());
    }

    #[test]
    fn parse_remote_info_reads_size_lines() {        let (download, installed) = parse_remote_info(
            "\n        ID: com.spotify.Client\n        Ref: \
             app/com.spotify.Client/x86_64/stable\n    Download Size: 9.8 MB\n   \
             Installed Size: 14.1 MB\n     Runtime: \
             org.freedesktop.Platform/x86_64/25.08\n",
        );
        assert_eq!(download.as_deref(), Some("9.8 MB"));
        assert_eq!(installed.as_deref(), Some("14.1 MB"));

        let (download, installed) = parse_remote_info("no sizes here\n");
        assert_eq!(download, None);
        assert_eq!(installed, None);
    }
}

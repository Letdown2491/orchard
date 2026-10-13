#!/usr/bin/env python3
"""Build the Orchard demo app as a local OCI image layout.

Reads demo/hello (the flatpak app tree: files/, metadata, export/) and
produces demo/oci-hello/ as a valid OCI layout plus demo/catalog.json
referencing it, exactly the schema orchard-indexd serves at /v1/catalog.

Format notes (validated against flatpak 1.18.4 sources and Fedora's live
registry, see crates/orchard-core/src/oci.rs):
  - the layer tar is the app tree rooted at `files/`, `metadata`, `export/`
  - the image config's `config.Labels` carries the flatpak keys:
      org.flatpak.ref       (required by flatpak)
      org.flatpak.metadata  (required; the app ini)
      org.flatpak.*         (timestamp, subject, sizes: optional)
  - manifest layer digest = sha256 of the compressed tar;
    config rootfs.diff_ids[0] = sha256 of the uncompressed tar

Flatpak reconstructs the ostree commit from layers plus labels at install
time, so no ostree repo or commit checksum is computed here.
"""

import base64
import gzip
import hashlib
import io
import json
import os
import stat
import sys
import tarfile
import time
from pathlib import Path

APP_ID = "rocks.orchard.Hello"
REF = f"app/{APP_ID}/x86_64/stable"
VERSION = "1.0.0"

ROOT = Path(__file__).resolve().parents[1]
APP_TREE = ROOT / "demo" / "hello"
LAYOUT = ROOT / "demo" / "oci-hello"
CATALOG = ROOT / "demo" / "catalog.json"

MEDIA_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
MEDIA_CONFIG = "application/vnd.oci.image.config.v1+json"
MEDIA_LAYER = "application/vnd.oci.image.layer.v1.tar+gzip"


def sha256(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def string_variant(text: bytes) -> str:
    """GVariant "v" wrapping a string, the encoding flatpak expects for
    org.flatpak.commit-metadata.* labels: the value bytes, NUL-terminated,
    then the type character (validated: GLib serialises a string variant as
    value + b"\\x00\\x00" + b"s" and reads it back)."""
    return base64.b64encode(text + b"\x00\x00s").decode()


def build_layer_tar() -> bytes:
    """Tar the app tree deterministically: sorted, mtime 0, uid/gid 0."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:

        def add_path(path: Path, arcname: str):
            st = path.lstat()
            info = tarfile.TarInfo(arcname)
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            if stat.S_ISDIR(st.st_mode):
                info.type = tarfile.DIRTYPE
                info.mode = 0o755
                tar.addfile(info)
                for child in sorted(path.iterdir()):
                    add_path(child, f"{arcname}/{child.name}")
            elif stat.S_ISLNK(st.st_mode):
                info.type = tarfile.SYMTYPE
                info.linkname = os.readlink(path)
                tar.addfile(info)
            else:
                info.size = st.st_size
                info.mode = 0o755 if st.st_mode & 0o111 else 0o644
                tar.addfile(info, open(path, "rb"))

        for entry in sorted(APP_TREE.iterdir()):
            add_path(entry, entry.name)
    return buf.getvalue()


def main() -> int:
    if not (APP_TREE / "metadata").is_file():
        print(f"missing app tree at {APP_TREE}", file=sys.stderr)
        return 1

    tar_bytes = build_layer_tar()
    gz_bytes = gzip.compress(tar_bytes, mtime=0)
    layer_digest = sha256(gz_bytes)
    diff_id = sha256(tar_bytes)

    tree_size = sum(
        p.stat().st_size for p in APP_TREE.rglob("*") if p.is_file()
    )

    metadata_ini = (APP_TREE / "metadata").read_bytes()
    config = {
        "created": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(0)),
        "architecture": "amd64",
        "os": "linux",
        "config": {
            "Labels": {
                "org.flatpak.ref": REF,
                "org.flatpak.metadata": metadata_ini.decode(),
                # The commit's xa.metadata must match the ini byte-for-byte
                # (flatpak's validate_commit_metadata compares them).
                "org.flatpak.commit-metadata.xa.metadata": string_variant(
                    metadata_ini
                ),
                "org.flatpak.installed-size": str(tree_size),
                "org.flatpak.download-size": str(len(gz_bytes)),
                "org.flatpak.timestamp": "0",
                "org.flatpak.subject": f"{APP_ID} {VERSION}",
                "org.flatpak.body": "Orchard demo release",
            }
        },
        "rootfs": {"type": "layers", "diff_ids": [diff_id]},
    }
    config_bytes = json.dumps(config, indent=1).encode()
    config_digest = sha256(config_bytes)

    manifest = {
        "schemaVersion": 2,
        "mediaType": MEDIA_MANIFEST,
        "config": {
            "mediaType": MEDIA_CONFIG,
            "digest": config_digest,
            "size": len(config_bytes),
        },
        "layers": [
            {
                "mediaType": MEDIA_LAYER,
                "digest": layer_digest,
                "size": len(gz_bytes),
            }
        ],
        # OCI manifest annotations mirror the labels; flatpak reads the
        # config labels but some tooling only shows manifest annotations.
        "annotations": dict(config["config"]["Labels"]),
    }
    manifest_bytes = json.dumps(manifest, indent=1).encode()
    manifest_digest = sha256(manifest_bytes)

    # Write the layout: blobs by digest, then index.json and oci-layout.
    for digest, blob in [
        (config_digest, config_bytes),
        (layer_digest, gz_bytes),
        (manifest_digest, manifest_bytes),
    ]:
        hex_part = digest.removeprefix("sha256:")
        dest = LAYOUT / "blobs" / "sha256" / hex_part
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(blob)

    (LAYOUT / "index.json").write_text(
        json.dumps(
            {
                "schemaVersion": 2,
                "manifests": [
                    {
                        "mediaType": MEDIA_MANIFEST,
                        "digest": manifest_digest,
                        "size": len(manifest_bytes),
                    }
                ],
            },
            indent=1,
        )
    )
    (LAYOUT / "oci-layout").write_text('{"imageLayoutVersion":"1.0.0"}\n')

    catalog = {
        "version": 1,
        "apps": [
            {
                "app_id": APP_ID,
                "name": "Orchard Hello",
                "summary": "Demo app published through Orchard",
                "description": (
                    "The Orchard demo: a shell script packaged as a flatpak, "
                    "distributed as a digest-pinned OCI artifact. Installing "
                    "it exercises the full Orchard path: catalog, digest "
                    "verification, and flatpak's direct OCI install."
                ),
                "publisher": {
                    "pubkey": "0000000000000000000000000000000000000000000000000000000000000000",
                    "nip05": None,
                },
                "version": VERSION,
                "flatpak_ref": REF,
                "location": f"oci:{LAYOUT}",
                "digest": manifest_digest,
                "metadata": metadata_ini.decode(),
                "notes": "First Orchard release. Grants: wayland, x11, full home access.",
                "published_at": 0,
            }
        ],
    }
    CATALOG.write_text(json.dumps(catalog, indent=1) + "\n")

    print(f"app tree:          {APP_TREE}")
    print(f"OCI layout:        {LAYOUT}")
    print(f"layer (gzip):      {layer_digest}")
    print(f"diff_id (tar):     {diff_id}")
    print(f"manifest digest:   {manifest_digest}")
    print(f"catalog:           {CATALOG}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

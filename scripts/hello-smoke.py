#!/usr/bin/env python3
"""Isolated Hello publication fixture used by hello-smoke.yml, never production."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / 'target/hello-smoke'
ARCHES = ('x86_64', 'aarch64')
REGISTRY = 'http://127.0.0.1:5000/hello'
RELAY = 'ws://127.0.0.1:8008'
IMAGE = 'localhost/orchard-dev-rust'


def run(args, **kwargs):
    return subprocess.run(args, check=True, timeout=600, **kwargs)


def container(binary, *args, signing=False):
    command = ['podman', 'run', '--rm', '--network', 'host',
               '-v', f'{ROOT}:/work:Z', '-w', '/work']
    env = os.environ.copy()
    if signing:
        # Public fixture key, restricted to this loopback-only test publication.
        env['ORCHARD_NSEC'] = '01' * 32
        command += ['-e', 'ORCHARD_NSEC']
    return run(command + [IMAGE, str(binary), *map(str, args)], env=env)


def ready(url):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            pass
    raise RuntimeError(f'service did not become ready: {url}')


def registry():
    run(['podman', 'run', '-d', '--name', 'hello-registry',
         '-p', '127.0.0.1:5000:5000', 'docker.io/library/registry:2'])
    ready('http://127.0.0.1:5000/v2/')


def manifest(arch):
    folder = OUT / arch
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / 'app.toml'
    path.write_text(f'''[app]
id = "rocks.orchard.Hello"
name = "Orchard Hello"
summary = "Isolated multi-architecture publication test"
[release]
version = "0.0.0-smoke"
arch = "linux-{arch}"
platforms = ["linux-x86_64", "linux-aarch64"]
[artifact]
tree = "tree"
''')
    return path.relative_to(ROOT)


def publisher(arch):
    return Path('target/hello-smoke') / arch / 'bin/orchard-publish'


def metadata(arch):
    return Path('target/hello-smoke') / arch / f'out/rocks.orchard.Hello-linux-{arch}.asset.json'


def upload(arch, binary_arch):
    container(publisher(binary_arch), manifest(arch), '--asset', metadata(arch),
              '--upload-only', '--registry', REGISTRY)


def prepare(arch):
    manifest(arch)
    folder = OUT / arch
    shutil.copytree(ROOT / 'demo/hello', folder / 'tree', dirs_exist_ok=True)
    meta = folder / 'tree/metadata'
    meta.write_text(meta.read_text().replace('/x86_64/', f'/{arch}/'))
    (folder / 'bin').mkdir(exist_ok=True)
    for source in ['orchard-publish', 'examples/hello_smoke']:
        shutil.copy2(ROOT / 'target/release' / source, folder / 'bin' / Path(source).name)
    # The first phase is offline and has no signing identity or hosting URLs.
    run(['podman', 'run', '--rm', '--network', 'none', '-v', f'{ROOT}:/work:Z',
         '-w', '/work', IMAGE, str(publisher(arch)), str(manifest(arch)),
         '--prepare', '--out', f'target/hello-smoke/{arch}/out'])
    registry()
    upload(arch, arch)


def publish():
    registry()
    for arch in ARCHES:
        upload(arch, 'x86_64')
    run(['podman', 'run', '-d', '--name', 'hello-relay',
         '-p', '127.0.0.1:8008:8080',
         '-v', f'{ROOT}/relay/config.toml:/usr/src/app/config.toml:ro,z',
         'localhost/orchard-relay'])
    ready('http://127.0.0.1:8008/')
    args = [manifest('x86_64')]
    for arch in ARCHES:
        args += ['--asset', metadata(arch)]
    # Explicit override avoids the production required-relay defaults. No --prove.
    container(publisher('x86_64'), *args, '--legacy-oci', '--relays', RELAY, signing=True)
    container('target/hello-smoke/x86_64/bin/hello_smoke', 'capture',
              'target/hello-smoke/events.jsonl')


def restore(arch):
    registry()
    # Re-host the exact same content-addressed bytes at the signed loopback URLs.
    # No re-signing or changes to the combined release in either install job.
    for other in ARCHES:
        upload(other, arch)
    container(f'target/hello-smoke/{arch}/bin/hello_smoke', 'download',
              'target/hello-smoke/events.jsonl', f'linux-{arch}',
              'target/hello-smoke/install.layout')


if __name__ == '__main__':
    os.chdir(ROOT)
    mode = sys.argv[1]
    if mode == 'publish':
        publish()
    elif mode in ('prepare', 'restore') and sys.argv[2] in ARCHES:
        globals()[mode](sys.argv[2])
    else:
        raise SystemExit('usage: hello-smoke.py prepare|restore ARCH, or publish')

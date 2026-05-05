# Setting up WPR-backed workloads for servoperf

`servoperf` can run benchmarks against a local Web Page Replay (WPR) archive
instead of reaching the live internet. This gives deterministic timing for
real-world page shapes (e.g. `cdn-huaweimossel`), isolating servo-side
improvements from network jitter.

This document covers one-time environment setup. Once these steps are
done, `servoperf bench <workload>` runs the record step automatically on
first use and then replays from the on-disk archive.

## 1. Install a Go toolchain

WPR's module requires Go ≥ 1.23. Ubuntu 24.04's `apt` ships Go 1.22, which
is too old. Use the upstream tarball:

```sh
curl -sL https://go.dev/dl/go1.25.9.linux-amd64.tar.gz -o /tmp/go.tgz
sudo tar -C /usr/local -xzf /tmp/go.tgz
/usr/local/go/bin/go version   # expect: go version go1.25.9 linux/amd64
```

Add `/usr/local/go/bin` to `PATH` or use the absolute path below.

## 2. Fetch the WPR source

```sh
GOTOOLCHAIN=go1.25.9 /usr/local/go/bin/go install \
    go.chromium.org/webpagereplay@latest
```

This writes sources into
`~/go/pkg/mod/go.chromium.org/webpagereplay@<version>/`. No binary is
produced — we need a one-line patch first.

## 3. Patch WPR's cert minting for Go 1.25

Go 1.25's `x509.CreateCertificate` rejects templates without
`SerialNumber`. WPR's `MintCertificate` omits it, so every TLS handshake
fails with `x509: no SerialNumber given`. Add a random 128-bit serial:

```sh
MOD_DIR="$(echo ~/go/pkg/mod/go.chromium.org/webpagereplay@*)"
chmod -R u+w "$MOD_DIR"
```

Edit `$MOD_DIR/src/webpagereplay/certs.go`:

1. Add `"math/big"` to the import block.
2. Inside `MintCertificate`, immediately before the `template :=
   x509.Certificate{ ... }` literal, insert:

   ```go
   serialNumberLimit := new(big.Int).Lsh(big.NewInt(1), 128)
   serialNumber, err := rand.Int(rand.Reader, serialNumberLimit)
   if err != nil {
       return nil, fmt.Errorf("generate serial number failed: %v", err)
   }
   ```

3. Add `SerialNumber: serialNumber,` as the first field of that
   `x509.Certificate{}` literal.

## 4. Build the `wpr` binary

`src/wpr.go` and `src/httparchive.go` are both `package main` inside the
same directory, so `go install` refuses. Build the one we actually use:

```sh
cd "$MOD_DIR"
mkdir -p ~/bin
GOTOOLCHAIN=go1.25.9 /usr/local/go/bin/go build -o ~/bin/wpr ./src/wpr.go
~/bin/wpr --help | head -3   # sanity
```

## 5. Install WPR assets in a writable location

WPR reads its root cert/key and the JS deterministic-overrides script
relative to its cwd. Keep them under `~/wpr/` so the binary is
portable:

```sh
mkdir -p ~/wpr
MOD_DIR="$(echo ~/go/pkg/mod/go.chromium.org/webpagereplay@*)"
cp "$MOD_DIR"/{deterministic.js,ecdsa_cert.pem,ecdsa_key.pem} ~/wpr/
```

The RSA cert and key that ship in the module are **1024-bit**, which
modern TLS stacks (including rustls — and therefore servoshell) reject
with `invalid peer certificate: BadSignature` / `EE certificate key too
weak`. Replace them with a freshly minted 2048-bit self-signed CA (the
subject matches the one WPR's original bundled cert used, but any
subject works — nothing in WPR parses it):

```sh
cd ~/wpr
openssl req -x509 -newkey rsa:2048 -days 3650 -nodes \
    -keyout wpr_key.pem -out wpr_cert.pem \
    -subj "/C=US/ST=California/L=San Francisco/O=WebPerfRSA Organization/OU=IT Department/CN=WebPerfRSACommonName/emailAddress=admin@example.com" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign"
chmod 600 wpr_key.pem
```

Verify: `openssl x509 -in wpr_cert.pem -noout -text | grep Public-Key`
should report `(2048 bit)`.

### Do I need to install this cert into the system trust store?

No. Workloads that use WPR set `servoshell_args =
["--ignore-certificate-errors"]` in their `[fixture]` TOML, which makes
servoshell accept WPR's self-signed leaf certs without any OS-level
trust-store wiring. That's the same mechanism the local `h1-multi` and
`h2-multi` fixtures use for their self-signed certs.

### Why the `--no-archive-certificates` flag?

WPR's default replay mode reuses the leaf certs that were minted at
recording time and stored inside the `.wprgo`. Those leaves were signed
by the recording-time root key, so once you regenerate `wpr_cert.pem`
/ `wpr_key.pem` (e.g. moving from the shipped 1024-bit RSA to the new
2048-bit one in step 5), the archived leaves no longer chain to the
on-disk root and every TLS handshake fails with
`InvalidCertificate(BadSignature)`. This error is **not** suppressed by
`--ignore-certificate-errors` in servoshell — that flag only ignores
the trust path (`UnknownIssuer`), not a cryptographic signature
mismatch.

`servoperf` passes `--no-archive-certificates` to WPR automatically
(see `fn spawn_wpr` in `src/fixtures.rs`), forcing fresh leaves at
play time using whatever root is currently on disk. You don't need to
do anything; this section just records *why* we set the flag.

## 6. (One-off, after environment changes) Tell servoperf where WPR is

The servoperf `wpr-replay` fixture looks for the binary and cert files
via these environment variables, with the defaults shown:

```sh
export SERVOPERF_WPR_BIN="$HOME/bin/wpr"
export SERVOPERF_WPR_CERT="$HOME/wpr/wpr_cert.pem"
export SERVOPERF_WPR_KEY="$HOME/wpr/wpr_key.pem"
```

If they're in the default locations, no configuration is needed. Add
the `export` lines to your shell rc if you want them persistent.

## 7. First-run record

The `wpr-replay` fixture auto-records on first use if the archive file
does not exist. Run an ordinary bench; servoperf detects the missing
archive, does a single record pass against the live origin, flushes,
then starts replay and runs the requested iterations:

```sh
servoperf bench cdn-huaweimossel --iterations 10 --bin /path/to/servoshell
```

The archive lands at `tools/servoperf/wpr-archives/<name>.wprgo`. It's
binary; inspect with `~/bin/wpr` tools if needed (see
`MOD_DIR/src/httparchive.go`).

To force a fresh recording, delete the archive file. To record multiple
pass variants (covering the common sources of page-level variability
like randomised analytics URLs), run `servoperf bench` three or four
times against the live origin before deleting; each additional record
pass merges into the same archive file.

## 8. Replaying to a HarmonyOS / OpenHarmony device

The WPR fixture works with `--ohos` too. The host runs `wpr` + the
`wpr_tunnel` shim exactly as for desktop; servoperf adds an
`hdc rport tcp:<tunnel_port> tcp:<tunnel_port>` so the device can
reach the host's loopback proxy:

```
device                                host
servoshell  --(CONNECT m.huaweimossel.com:443)-->  wpr_tunnel:4480
                via 127.0.0.1:4480 → rport →               │
                                                           ▼
                                                       wpr:4443
                                                  (replays archive)
```

What's plumbed automatically:

- `Fixture::ports_to_forward()` returns `[tunnel_port]` for `WprReplay`.
  servoperf installs the rport, the device sees `127.0.0.1:<tunnel_port>`
  bridged to the host, and the proxy URI works unchanged.
- The OHOS args translator routes the workload's
  `--ignore-certificate-errors` to `--psn=--ignore-certificate-errors`
  on `aa start`. It also injects the proxy URI as servoshell prefs
  (`network_https_proxy_uri` / `network_http_proxy_uri`) — env-var
  inheritance doesn't survive the `aa start` boundary.

**Auto-record on OHOS** works the same as on desktop: when the
`.wprgo` archive is missing, `servoperf` runs a one-shot record pass
through `aa start` against the live origin (via `wpr_tunnel` +
`hdc rport`) and then flips into replay mode for the iteration loop.
The window is 45 s by default — enough to capture page load and the
lazy-loaded image tail — and is overridable with
`--ohos-record-seconds <n>`. The driver is selected automatically by
[`build_record_driver`](../src/cmd/bench.rs):

- desktop target → `LocalServoshellDriver { bin: <servoshell> }`
- OHOS target    → `OhosRecordDriver { target, record_seconds }`

After recording, `servoperf` requires the archive to be at least
100 KB (a tiny archive almost always means "device couldn't reach the
proxy" — bundle not installed, or `hdc rport` not listening).

End-to-end recipe (no prior desktop record needed):

```sh
# 1. Build + install the OHOS hap with hitrace tracing.
( cd servo
  ./mach build --ohos --flavor=harmonyos --profile=release \
               --features tracing,tracing-hitrace
  ./mach install --ohos --flavor=harmonyos --profile=release )

# 2. Bench against the device. servoperf records the missing archive
#    on first run, then replays. Subsequent runs replay only.
servoperf bench cdn-huaweimossel --ohos --iterations=10
# Override the record window for slow pages: --ohos-record-seconds=90
```

## 9. Troubleshooting

- **`listen tcp 127.0.0.1:4443: bind: address already in use`** — a
  previous WPR or tunnel didn't exit cleanly. `pkill -x wpr` and
  re-run.
- **`invalid peer certificate: BadSignature`** — *either* your WPR is
  still using the shipped 1024-bit CA (redo step 5), *or* WPR is
  replaying archived leaves signed by an older root key. The current
  `spawn_wpr` passes `--no-archive-certificates`, so this should be a
  non-issue with `servoperf`-launched WPR. If you're invoking `wpr`
  by hand, add the flag yourself; otherwise rebuild a fresh archive
  after regenerating the CA.
- **`x509: no SerialNumber given`** — the patch in step 3 didn't land.
  Re-edit `certs.go` and re-run `go build`.
- **`fixture_server binary not found`** — the servoperf binary and the
  fixture helper binaries need to live in the same `target/<profile>`
  directory. Run `cargo build -p servoperf --bins` (all binaries, not
  just `servoperf`).
- **`context deadline exceeded` during record, but replay works** —
  `/etc/hosts` has a leftover `127.0.0.1 <origin>` entry from an
  earlier attempt. Remove it; WPR-replay works without it.
- **Servoshell silently hangs for ~30s at startup, no fetches arrive at
  WPR** — `https_proxy` / `http_proxy` env vars weren't inherited by
  the servoshell child, or they point at the wrong port. The
  `wpr-replay` fixture sets these automatically; if you're running
  servoshell by hand, use `https_proxy=http://127.0.0.1:<tunnel_port>`.

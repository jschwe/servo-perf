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

## 8. Troubleshooting

- **`listen tcp 127.0.0.1:4443: bind: address already in use`** — a
  previous WPR or tunnel didn't exit cleanly. `pkill -x wpr` and
  re-run.
- **`invalid peer certificate: BadSignature`** — your WPR is still
  using the shipped 1024-bit CA. Redo step 5.
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

# webarchive-downloader-rust

[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Coverage](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=coverage)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Bugs](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=bugs)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Vulnerabilities](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=vulnerabilities)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Code Smells](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=code_smells)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Duplicated Lines](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=duplicated_lines_density)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Maintainability](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=sqale_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Reliability](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=reliability_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Security](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=security_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Lines of Code](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=ncloc)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)
[![Technical Debt](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_webarchive_downloader_rust&metric=sqale_index)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_webarchive_downloader_rust)

Download a static copy of a website from the Internet Archive Wayback Machine.

The Cargo package and executable use kebab-case:

```sh
webarchive-downloader-rust another.by
```

Inside Rust code, Cargo exposes the library crate as `webarchive_downloader_rust` because Rust identifiers cannot contain hyphens. That split is normal Rust practice.

## Status

This is an early but working Rust CLI. It:

- queries the Wayback CDX API for archived captures
- keeps the latest capture per original URL by default
- downloads files sequentially to keep memory use low and avoid hammering the Internet Archive
- retries transient Wayback CDX failures indefinitely with backoff and diagnostic retry logs
- logs each URL before downloading it
- skips existing files by default so interrupted runs can resume
- writes through temporary files and renames atomically after success
- handles Ctrl-C by stopping new downloads and reporting the partial output
- reports the output folder size and 10 biggest files at the end
- falls back to older captures when the latest HTML capture is only a soft redirect
- skips obvious session/query/placeholder-noise URLs by default
- streams binary files to disk
- downloads explicitly linked binary archives/installers and static assets from related subdomains when they fit the configured size cap
- rewrites common HTML and CSS links to local relative paths
- can repair an existing output directory by fetching missing static assets that are present in Wayback
- queries Wayback directly for missing local static assets that were not present in the initial CDX result
- creates conservative local aliases for obvious static asset filename variants, such as `screen4.jpg` to an archived `screenshot4.jpg`
- removes broken local resource references after recovery finishes without deferred Wayback lookups
- removes generated local anchor links when their target was not captured
- validates generated local links after download and reports missing files
- writes to `public/` by default, which matches GitLab Pages conventions

## Install

From this repository:

```sh
cargo install --path .
```

Or run without installing:

```sh
cargo run --release -- another.by
```

## Usage

Download one host into `public/`:

```sh
webarchive-downloader-rust another.by
```

Download a whole domain, including subdomains, only when you explicitly want a
multi-host archive:

```sh
webarchive-downloader-rust another.by --match-type domain
```

Choose an output directory. If a previous run was interrupted, run the same command again and already completed files will be skipped:

```sh
webarchive-downloader-rust another.by --output public
```

Existing non-empty files are skipped by default, and downloads are written
through temporary files before being renamed into place. If Wayback's CDX index
points at a snapshot that now returns a permanent missing status such as 404, the
file is reported as an unavailable snapshot and the run continues.

Inspect selected captures without downloading:

```sh
webarchive-downloader-rust another.by --list --limit 20
```

Validate an existing output directory without downloading or modifying files:

```sh
webarchive-downloader-rust --validate-only --output public --strict-validate-links
```

Repair an existing output directory. This fetches only recoverable missing static
assets from Wayback, then reports assets that are not archived or exceed the size
cap:

```sh
webarchive-downloader-rust another.by --repair-output --output public
```

Download an older museum snapshot:

```sh
webarchive-downloader-rust another.by --to 20141231 --strategy latest
```

Force a full refresh of already downloaded files:

```sh
webarchive-downloader-rust another.by --overwrite
```

## GitHub Actions archiver

This repository includes a manual `Archive website` GitHub Actions workflow for
small sites. Open the Actions tab, choose `Archive website`, click `Run workflow`,
and provide a domain or URL. The workflow builds the downloader, writes the site
to `public/`, packs `website-archive.tar.gz`, and stores it as a downloadable
artifact for 7 days.

For repeated runs, set the `ARCHIVE_TARGET` repository variable and leave the
manual `target` input empty. Optional variables and secrets:

- `ARCHIVE_TARGET`: default domain or URL to archive.
- `WAYBACK_SSH_DESTINATION`: default SSH fallback destination or destinations,
  such as `ubuntu@151.145.94.114`. Use commas, spaces, or newlines to provide
  multiple fallbacks.
- `WAYBACK_SSH_PRIVATE_KEY`: private key secret used when SSH fallback is
  configured.

GitHub-hosted runners can run one job for up to 6 hours, so this workflow is
intended for modest sites, not multi-day domain downloads. Use a self-hosted
runner or a VPS for larger preservation runs.

During a long run, press Ctrl-C once to stop after current requests finish. Already
completed files stay in place, incomplete temporary files are not promoted, and the
final report still prints the output size and biggest files. Press Ctrl-C a second
time to exit immediately.

Useful options:

```text
--match-type domain|host|prefix|exact
--strategy latest|earliest
--from YYYYMMDDhhmmss
--to YYYYMMDDhhmmss
--limit N
--validate-only
--repair-output
--overwrite
--no-rewrite
--no-validate-links
--strict-validate-links
--max-extra-download-size-mib N
--timeout-seconds N
--ssh USER@HOST  (repeatable)
--user-agent "webarchive-downloader-rust/0.1 your-email@example.com"
```

## GitLab Pages

For a museum repository, commit the downloaded `public/` directory and add a Pages job like this:

```yaml
pages:
  stage: deploy
  script:
    - test -d public
  artifacts:
    paths:
      - public
  only:
    - main
```

Then run:

```sh
webarchive-downloader-rust another.by --output public
```

Review the result locally, commit `public/`, and push to GitLab.

## Notes

The default `--match-type host` asks the CDX API for one host only. This produces
a root-level static site that is easier to host on GitLab Pages. If you use
`--match-type domain`, subdomains are written under `_hosts/<hostname>/` so their
paths cannot collide with the primary site.

Even with `--match-type host`, the downloader follows explicit binary download
links and static assets to related subdomains, such as
`downloads.example.com/file.exe` or `downloads.example.com/preview/shot.jpg`,
without crawling the whole subdomain. These extra files are stored under
`_hosts/<hostname>/`. The default cap is 1 byte under 100 MiB per extra download
so the output stays below common Git hosting per-file limits. Use
`--max-extra-download-size-mib 0` to disable this pass.

The downloader uses Wayback `id_` snapshot URLs so it gets archived bytes with minimal Wayback rewriting, then performs local HTML/CSS rewrites itself. The rewrite pass handles ordinary links and resources, `srcset`, inline CSS, common JavaScript URL strings, old image rollover handlers, dropdown `option` values that contain URLs, meta-refresh targets, and legacy applet/object/param resource attributes.

If the latest HTML capture is only a meta-refresh or JavaScript redirect, the
downloader tries older exact captures for that URL. During that fallback it also
skips captures that no longer look like the requested site, for example a reused
domain whose page does not mention the original site name.

The downloader skips obvious session/query/placeholder-noise URLs by default,
such as `sid=...`, `PHPSESSID=...`, `ticket=...`, empty query strings, forum
login/posting/profile/search/member-list action pages, forum sort/highlight/mark
actions, and common cPanel/hosting placeholder paths like `cgi-sys/`, `img-sys/`,
`sys_cpanel/`, `cgi-bin/`, and root `welcome.png` IIS placeholder images.

After post-processing, the downloader scans local references in generated HTML,
CSS, and common inline JavaScript strings, then reports references whose target
file is missing. It also reports image elements that still have neither `src`
nor `srcset`, because those cannot render but do not have a target path to
validate. By default this is a warning so partial museum builds can still
finish. Use `--strict-validate-links` to return exit code 2 when missing local
links or source-less images remain, or `--no-validate-links` to skip the pass.

The repair pass only downloads real files that Wayback has captured. It first
tries the site CDX result, then queries likely original URLs for each still
missing static asset. Transient Wayback failures such as timeouts, connection
errors, HTTP 429, and server errors are retried indefinitely with capped
exponential backoff, so long preservation runs do not require manual reruns just
because the Internet Archive was temporarily unavailable. This applies both to
CDX lookups and archived snapshot downloads. When Wayback does not provide a
`Retry-After` header, the backoff grows to a one-day cap. Retry logs include the
attempt number, elapsed retry time, and underlying network cause. Repeated retry
messages are compacted after the first few attempts, and long TCP connect
failures print a periodic diagnostic telling the user to check network, firewall,
proxy, VPN, or route access to `https://web.archive.org/`. The default request
timeout is 900 seconds and can be changed with `--timeout-seconds`. CDX retries
share a process-wide cooldown, so when Wayback starts returning 429s or TCP-level
failures, later primary and recovery CDX lookups pause before sending more
requests.

If local Wayback access is blocked for a long time, pass `--ssh USER@HOST` to
allow the downloader to retry through that host. Repeat `--ssh` to provide
multiple fallbacks; they are tried in order as the current route fails. SSH
tunnels are started lazily only after a Wayback request hits a retryable failure
such as a timeout, HTTP 429, HTTP 403, or server error. The fallback uses OpenSSH
dynamic forwarding (`ssh -N -D`) and requires non-interactive key or SSH agent
authentication; configure host keys and jump hosts in your normal SSH config. If
recovery finishes without deferred static assets, remaining broken local
resource references are removed instead of inventing placeholder content.

For large domains, use `--from`, `--to`, and `--limit` to keep runs focused. The Internet Archive is a shared service, so the downloader intentionally fetches archived files one at a time.

## References

- Internet Archive Wayback CDX Server API: https://github.com/internetarchive/wayback/blob/master/wayback-cdx-server/README.md
- waybackpack on Python: https://github.com/jsvine/waybackpack
- wmd-straw on Ruby: https://github.com/StrawberryMaster/wayback-machine-downloader
- Wayback Machine Downloader on JS: https://github.com/birbwatcher/wayback-machine-downloader

The code is generated by LLM gpt-5.5 xhigh.

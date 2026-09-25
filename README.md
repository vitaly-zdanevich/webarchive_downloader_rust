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
- logs each capture timestamp, URL, and destination before downloading it
- preserves existing files by default while re-reading archived HTML/CSS for discovery on resumed runs
- writes through temporary files and renames atomically after success
- handles Ctrl-C by stopping new downloads and reporting the partial output
- reports the output folder size and 10 biggest files at the end
- prints elapsed runtime followed by the finish date and time in the machine's local timezone
- falls back to older captures when the latest HTML capture is only a soft redirect
- skips duplicate session-only URLs and hosting placeholder captures by default
- streams binary files to disk
- recursively discovers linked pages and resources on related hosts, including pages omitted from the initial CDX result
- rewrites common HTML and CSS links to local relative paths
- can repair an existing output directory by fetching missing static assets that are present in Wayback
- queries Wayback directly for missing local static assets that were not present in the initial CDX result
- creates conservative local aliases for obvious static asset filename variants, such as `screen4.jpg` to an archived `screenshot4.jpg`
- creates topic aliases only within the destination forum directory, keeping relative resources intact; shared post IDs or titles never establish equivalence across subdomains or separate forums
- retains unresolved links and resource references instead of hiding gaps in the local mirror
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

Download a domain, including its archived subdomains, into `public/`:

```sh
webarchive-downloader-rust another.by
```

Narrow the initial CDX search to one host when a domain-wide inventory is not
needed (linked discovery can still follow references to related subdomains):

```sh
webarchive-downloader-rust another.by --match-type host
```

Choose an output directory. If a previous run was interrupted, run the same command again and already completed files will be skipped:

```sh
webarchive-downloader-rust another.by --output public
```

Existing non-empty files are not overwritten by default. With rewriting and
linked discovery enabled, their archived HTML/CSS sources are fetched again to
discover missing content; existing binary files are skipped without refetching.
New downloads are written through temporary files before being renamed into
place. If Wayback's CDX index
points at a snapshot that now returns a permanent missing status such as 404,
the downloader tries other indexed captures before reporting the file as
unavailable.

The completion footer includes the calculated weekday:

```text
elapsed time: 8h 28m 59s
Finished at 2026 September 11, Friday, 23:59
```

Inspect selected captures without downloading:

```sh
webarchive-downloader-rust another.by --list --limit 20
```

Validate an existing output directory without downloading or modifying files:

```sh
webarchive-downloader-rust --validate-only --output public --strict-validate-links
```

Fix navigation links to HTML files already downloaded, without contacting Wayback:

```sh
webarchive-downloader-rust --repair-local-links --output public --strict-validate-links
```

This explicitly modifies existing HTML files, so keep a backup first. It repairs
local anchor/area links such as `faq.php?sid=...` only when `faq.php.html` exists
and is nonempty. It preserves fragments and unrelated bytes, skips meaningful
queries, forms, external links and pages with a `<base>` URL, and never creates
placeholder content. Repeating the repair makes no further changes. Remaining
missing links still produce exit code 2 with `--strict-validate-links`.

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

Even with `--overwrite`, a recognized HTML error/redirect cannot replace an
existing nonempty file. Failed transfers also leave the existing file intact.

### Maximum Content Recovery

To include indexed subdomains and recursively follow archived links without a
file-size cap or date limit, use:

```sh
webarchive-downloader-rust smallrockets.com --recover-existing \
	--output /tmp/smallrockets-preservation
```

No extra flag is needed to include indexed subdomains, enable linked discovery,
or allow unlimited file sizes.
Do not pass `--limit`, `--no-rewrite`, or `--max-extra-download-size-mib 0` for
this mode. Add `--ssh USER@HOST` only for a working, trusted SSH route. A normal
run against an existing output directory reuses local pages and saved original
references without replacing files. `--recover-existing` additionally retries
recognizable local HTML error pages and replaces them only after a usable
capture is found. Back up the output first and keep using the same directory;
do not delete it between recovery attempts. `--repair-output` alone only
recovers static assets.

Captures and replay failures are recorded in `.wayback-state/captures.jsonl`
inside the output directory. The append-only journal preserves original URLs,
capture timestamps, raw outgoing references, and failure reasons. Keep it with
the archive: local query hashes cannot reconstruct their original URLs. It is
recovery metadata, not a backup of file contents or a complete capture inventory.
Original references are retained even when linked downloads are disabled.
Binary transfers are still validated by the
download stream; the journal records their terminal failures, not successful
binary fallback provenance.

Resume uses local references and the journal without replaying healthy files.
An interrupted final journal entry is ignored with a warning; earlier
successful entries remain usable.

Exact CDX responses are reused across recovery passes within one run. Empty
results are not persisted between runs, and CDX failures are never cached as
missing content. Forum error detection uses structured message containers and
does not treat quotations inside discussions as missing-content pages. Missing
directory-index links can reuse a usable sibling `index.htm`, without replacing
existing files or inventing page content. Recovery remains limited to content
that Wayback can actually return; the tool cannot recreate unarchived posts.
Recovery stays within related hosts and the requested date range, fetches from
Wayback rather than live sites, and stops following cycles already visited in
the current run. Domain-wide discovery can take longer than an explicit
host-scoped run.

This is not a complete historical backup of every capture: one selected version
per mapped URL is saved. Content never captured by Wayback, inaccessible
captures, and dependencies generated only by executing scripts can remain missing.

## GitHub Actions archiver

This repository includes a manual `Archive website` GitHub Actions workflow for
small sites. Open the Actions tab, choose `Archive website`, click `Run workflow`,
and provide a domain or URL. The workflow builds the downloader, writes the site
to `public/`, packs `website-archive.tar.gz`, and stores it as a downloadable
artifact for 7 days. Its `match_type` input defaults to `domain`, matching the CLI.

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
--repair-local-links
--repair-output
--overwrite
--recover-existing
--no-rewrite
--no-validate-links
--strict-validate-links
--max-extra-download-size-mib N
--timeout-seconds N
--ssh USER@HOST  (repeatable)
--user-agent "webarchive-downloader-rust/0.4.2 your-email@example.com"
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

The default `--match-type domain` asks the CDX API for the target and its
subdomains to maximize content discovery, including pages not linked from the
primary site. Subdomains are written under `_hosts/<hostname>/` so their paths
cannot collide with the primary site. Use `--match-type host` to narrow the
initial CDX search to one host.

Even with `--match-type host`, the downloader follows linked pages and resources
on related hosts. Each recovered HTML/CSS document contributes another wave of
references, including extensionless pages, `srcset` images, CSS `@import` rules,
and nested stylesheet resources. Protocol-relative URLs and Wayback-wrapped
URLs retain their original queries. With `--match-type host`, only referenced
subdomain URLs are queried; that mode skips the domain-wide CDX inventory but
does not prohibit related-host downloads.
Extra subdomain files are stored under
`_hosts/<hostname>/`. By default there is no size cap for preservation
completeness. Use `--max-extra-download-size-mib N` to cap each extra download
at `N` MiB, or `--max-extra-download-size-mib 0` to disable this pass.
CDX records that omit their archived length are still attempted; an explicit
size cap is checked against the response bytes before saving the file; binary
downloads are stopped during streaming when they exceed the cap.

Primary CDX discovery, extra linked files, and static asset recovery all retry
patiently because preservation completeness matters more than short run time.
If Wayback starts returning 429s, 5xx responses, timeouts, or TCP-level
connection failures, the downloader backs off with a shared cooldown and keeps
waiting. CDX requests begin five seconds apart. Each 429 doubles that minimum
spacing up to five minutes, while sustained successful requests gradually reduce
it again.

The downloader uses Wayback `id_` snapshot URLs so it gets archived bytes with minimal Wayback rewriting, then performs local HTML/CSS rewrites itself. The rewrite pass handles ordinary links and resources, `srcset`, inline CSS, common JavaScript URL strings, old image rollover handlers, dropdown `option` values that contain URLs, meta-refresh targets, and legacy applet/object/param resource attributes.

If the latest HTML capture is only a meta-refresh or JavaScript redirect, the
downloader tries older exact captures for that URL. During that fallback it also
skips captures that no longer look like the requested site, for example a reused
domain whose page does not mention the original site name.

If a selected replay returns 404, 410, or 451 even though CDX listed it as a
successful capture, the downloader tries alternate captures of the same URL.
This applies to buffered pages and streamed files.
If no usable replay remains during static-asset recovery, the asset is reported
as unavailable and recovery continues, including when fetching an alias source.
Both normal downloads and `--repair-output` still reach validation and the final
folder-size summary; unresolved references are preserved. Lookup and filesystem
errors are not treated as missing snapshots.
Repeated body-read or decoding failures also trigger alternate-capture lookup.
Alternate body recovery and static-asset evidence searches have no twenty-capture
limit; all eligible indexed candidates can be considered.

The downloader canonicalizes volatile query parameters such as `sid=...`,
`PHPSESSID=...`, `ticket=...`, and forum sort/highlight/mark parameters so
session-only variants map to one local file. Forum profile, member-list, search,
login, and posting pages are retained when Wayback captured them because they
can contain historical content. Common cPanel/hosting placeholder paths such as
`cgi-sys/`, `img-sys/`, `sys_cpanel/`, `cgi-bin/`, and root `welcome.png` IIS
placeholder images are skipped.

After post-processing, the downloader scans local references in generated HTML,
CSS, and common inline JavaScript strings, then reports references whose target
file is missing. It also reports image elements that still have neither `src`
nor `srcset`, because those cannot render but do not have a target path to
validate. By default this is a warning so partial museum builds can still
finish. Use `--strict-validate-links` to return exit code 2 when missing local
links, source-less images, or recognized unusable HTML pages remain, or
`--no-validate-links` to skip the pass.
The summary separates missing link occurrences from distinct missing target
paths. `failed: 0` means no terminal download errors, not a complete mirror.
`retained unusable captures` counts newly saved HTML placeholders for which no
usable fallback was found; their archived bytes are kept instead of deleted.
`unusable HTML files` checks the final output, including files retained from
earlier runs. This is a conservative detector, not a guarantee that every other
saved page contains useful content.

The repair pass only downloads real files that Wayback has captured. It first
tries the site CDX result, then queries likely original URLs for each still
missing static asset. Transient Wayback failures such as timeouts, connection
errors, HTTP 429, and server errors are retried indefinitely with capped
exponential backoff, so long preservation runs do not require manual reruns just
because the Internet Archive was temporarily unavailable. This applies both to
CDX lookups and archived snapshot downloads. When Wayback does not provide a
`Retry-After` header, the backoff grows to a one-day cap. Retry logs include the
attempt number, active route, elapsed retry time, exact delay in seconds, a
readable approximation for long delays, and the underlying network cause.
Repeated retry messages are compacted after the first few attempts, and long TCP
connect failures print a periodic diagnostic telling the user to check network,
firewall, proxy, VPN, or route access to `https://web.archive.org/`. The default
request timeout is 900 seconds and can be changed with `--timeout-seconds`; this
controls one HTTP request, not the delay after a 429. CDX retries share a
process-wide cooldown, so later primary and recovery CDX lookups pause before
sending more requests. Once Wayback accepts a CDX request, that successful
response resets the accumulated exponential backoff so a stale one-day local
delay does not remain in force. This only resets local state; it does not clear
Wayback's server-side rate limit, and a later 429 starts backoff again while the
slower adaptive request spacing remains active. The one-day value caps a single
wait rather than the total retry period. After a day-long cooldown completes,
the downloader resets the exponential penalty for shorter recovery probes but
retains the slower request spacing. It continues until Wayback responds or the
run is cancelled.

If local Wayback access is blocked for a long time, pass `--ssh USER@HOST` to
allow the downloader to retry through that host. Repeat `--ssh` to provide
multiple fallbacks; they are tried in order as the current route fails. SSH
tunnels are started lazily only after a Wayback request hits a retryable failure
such as a timeout, HTTP 429, HTTP 403, or server error. The fallback uses OpenSSH
dynamic forwarding (`ssh -N -D`) and requires non-interactive key or SSH agent
authentication; configure host keys and jump hosts in your normal SSH config.
Tunnel startup waits up to 60 seconds. Failed starts are terminated cleanly, and
temporarily failed SSH routes become eligible again after a cooldown instead of
remaining disabled until the downloader restarts. References whose targets are
not available in Wayback remain in the generated files and are reported by link
validation rather than being deleted.
Direct HTTP 5xx responses receive two retries before starting SSH, avoiding a
sixty-second tunnel startup for brief server failures. HTTP 403/429 can switch
immediately. Failed SSH startup announces its cooldown once; requests made
during that cooldown do not repeat the same message.

For large domains, use `--from`, `--to`, and `--limit` to keep runs focused. The Internet Archive is a shared service, so the downloader intentionally fetches archived files one at a time.

## References

- Internet Archive Wayback CDX Server API: https://github.com/internetarchive/wayback/blob/master/wayback-cdx-server/README.md
- waybackpack on Python: https://github.com/jsvine/waybackpack
- wmd-straw on Ruby: https://github.com/StrawberryMaster/wayback-machine-downloader
- Wayback Machine Downloader on JS: https://github.com/birbwatcher/wayback-machine-downloader

The code is generated by LLM gpt-5.5 xhigh.

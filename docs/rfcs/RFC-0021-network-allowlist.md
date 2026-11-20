# RFC-0021: A per-run list of hosts the sandbox may reach

| | |
|---|---|
| Status | built 2026-11-20 for issue #324, except the log record in section 3; see section 10 |
| Date | 2026-11-20 |
| Depends on | RFC-0009, RFC-0010 |
| Scope | `--allow-net`, a filtering proxy, and how each sandbox is limited to it |

## 1. What it is for

The workspace sandbox denies the network, which is the right default. A headless
run therefore cannot install dependencies or reach a forge, and every caller who
wants that rebuilds the same glue around Dino: fetch first, push after. Issue
#324 asks for `--allow-net registry.npmjs.org,crates.io,github.com`, a list of
hosts that processes may reach, without going as far as `--sandbox none`.

## 2. Why it needs a proxy

Neither sandbox can name a host.

- **Landlock** (ABI 4) restricts TCP connect and bind by port, to any address.
  Allowing 443 allows every HTTPS server in the world, which is what the
  `packages` tool's fetching mode already does, and is not a list of hosts.
- **Seatbelt** matches `remote tcp "host:port"` where host is `*` or
  `localhost`. It cannot match `github.com`, and an IP would be wrong the moment
  a CDN moved.

A hostname is only known at the point a client asks for it. So Dino runs a
small HTTP proxy for the session, the sandbox is allowed to reach that proxy and
nothing else, and the proxy decides by name.

## 3. Shape

```text
 sandboxed process --HTTPS_PROXY--> dino's proxy --listed host only--> internet
   (no route out)                    (session process, unconfined)
```

- `--allow-net host[,host...]` on `dino`, `dino run` and `dino serve`, and
  `allow_net = [...]` in the user file. Never in a project file: a repository
  may not widen the sandbox (RFC-0015), and this would be the widest key it
  could set.
- A host is an exact name (`crates.io`) or a suffix (`*.npmjs.org`). Port 443
  unless written `host:port`.
- The proxy accepts `CONNECT host:port` only. It resolves the name itself and
  connects only when the name is on the list. Plain HTTP is refused; every
  registry in the issue serves HTTPS. Since the proxy resolves, the sandbox needs
  no DNS at all, and keeps none.
- The child gets `HTTPS_PROXY`, `https_proxy`, `HTTP_PROXY`, `http_proxy` and
  `ALL_PROXY` pointing at it, and `NO_PROXY` unset. npm, pnpm, cargo, git over
  HTTPS, pip, go and curl all honour these.
- A client that ignores them fails, because the sandbox still has no other
  route. That is the property worth having: the failure is closed.
- Each CONNECT is written to the session log with the host and the verdict, so
  `dino trace` shows what a run reached and what it was refused.
- The banner says it: `network: HTTPS to 3 host(s) through dino's proxy`.

## 4. Limiting each sandbox to the proxy

**macOS.** The profile gains `(allow network-outbound (remote tcp
"localhost:PORT"))` for the proxy's port and nothing else. No UDP 53 and no
mDNSResponder, unlike `fetch_rules`, since the proxy resolves.

*As built:* the workspace profile already allows `remote ip "localhost:*"`, so
the proxy is reachable without a rule of its own and only the child's
environment changes. Narrowing loopback to the one port would also take away
the local servers tests start, which is a separate decision.

**Linux with a loopback namespace**, the usual case. The child is in a network
namespace of its own (`dino-confine --loopback`), where 127.0.0.1 is not the
session's 127.0.0.1, so a TCP proxy in the session cannot be reached. Pathname
Unix sockets are not scoped to a network namespace, so:

1. The proxy listens on a Unix socket in the session's temporary directory as
   well as on TCP.
2. `dino-confine` gains `--proxy <socket>`. After bringing `lo` up it forks a
   relay that listens on 127.0.0.1 inside the namespace and copies each
   connection to the socket, then confines itself and execs the program as now.
3. `HTTPS_PROXY` names the relay's port.

The relay runs inside the namespace and under the same Landlock ruleset, so it
reaches nothing the child could not.

**Linux without user namespaces.** Landlock would have to allow connect to the
proxy's port on every address, so a process could reach any host that happens
to listen on that port. That is not a list of hosts. `--allow-net` is refused
here with that reason, rather than given a weaker meaning under the same flag.

## 5. What it does not do

- **It does not make `git push` work by itself.** The sandbox denies reads of
  credential paths on purpose (RFC-0009 section 2): `~/.config/gh`, the macOS
  keychains, `.git-credentials`. A push over HTTPS through the proxy reaches
  GitHub and is refused for want of a token. Pushing still belongs to an
  unconfined call the person approves, or to the caller after Dino exits. This
  RFC does not change that, and should not be read as having done so.
- **It does not cover SSH.** `git@github.com:` is not HTTP and cannot pass a
  CONNECT proxy without a `ProxyCommand` the child would have to be given.
- **It is not content filtering.** The proxy sees a host name and then
  ciphertext. Allowing `github.com` allows any repository, gist or issue on it.
- **Cargo stays `--offline`.** The `cargo` tool's fetch already has its own path
  and needs `CARGO_HOME` writable (RFC-0009 section 8). Joining the two is a
  separate change.

## 6. The cost, stated plainly

The guarantee in RFC-0009 is that a sandboxed process may learn a secret but
cannot transmit it. **Every host on the list is a place a process can send what
it read.** A list of registries is a small hole; a forge that accepts uploads
from anyone is a large one. `dino config` and the banner name the hosts, and the
README says this sentence, so nobody adds `github.com` thinking it only lets
packages in.

## 7. Alternatives

- **TCP 443 to everywhere**, as `packages` uses. Already exists for package
  fetches with scripts disabled. As a general mode it is `--sandbox none` for
  anything that speaks HTTPS.
- **Resolve the names at start and allow the IPs.** Seatbelt cannot express it,
  Landlock cannot either, and CDN addresses move during a run.
- **Fetch before, push after**, as the reporter does now. Works today, and is
  the right answer for pushing, per section 5.
- **Extend `packages` to more ecosystems.** Narrower and safer for dependencies,
  and worth doing anyway; it does not help a test that needs a live service.

## 8. The order of work

1. Check the two facts this design leans on and has not measured: that Seatbelt
   accepts `remote tcp "localhost:PORT"` and refuses everything else with it,
   and that a confined process in the loopback namespace can connect to a
   pathname Unix socket outside the workspace under the Landlock ruleset.
2. The proxy, with tests for an allowed host, a refused host, a suffix match,
   plain HTTP refused and a name that resolves to loopback refused.
3. macOS, with an escape test: `curl` to a listed host succeeds, to an unlisted
   one and with the proxy variables cleared fails.
4. Linux, the relay in `dino-confine`, and the same tests in the `installed` CI
   job.
5. The flag, the user-file key, the banner, the log record and the README.

## 9. Open questions

- Whether the proxy should refuse a listed name that resolves to a private or
  loopback address, as a guard against a DNS answer that points back inside the
  machine. Proposed: yes.
- Whether `dino eval` should ever accept a list. Proposed: no, as with every
  other setting that would make two runs of a case differ.

## 10. As built, 2026-11-20

`--allow-net` and `allow_net` in the user file, refused from a project file and
never read by `dino eval`. The proxy is `dino_sandbox::Proxy`, standard library
only, and refuses a listed name that resolves to a loopback, private, tailnet
or link-local address, which settles the first question in section 9. On Linux
the relay is `dino-confine` run again as its own child after it has confined
itself, so it is inside the namespace and under the ruleset without `fork` or
any `unsafe`; it dies with the program by a parent-death signal.

Measured, with the ignored test `allow_net_reaches_a_listed_host_and_nothing_else`
in `dino-exec`, on macOS and on Debian 13 with kernel 6.12: a listed host
answers 200, an unlisted one gets 403 from the proxy, and the same request with
the proxy switched off fails to resolve. The two facts in section 8 step 1 held
on both.

Not built: the per-CONNECT record in the session log. A refusal reaches the
model as the proxy's 403 in the command's output, with a note saying it is
policy. Found on the way, and not this RFC's to fix: a confined process on
Linux can connect to any pathname Unix socket on the machine, in either network
mode, since Landlock does not govern that.

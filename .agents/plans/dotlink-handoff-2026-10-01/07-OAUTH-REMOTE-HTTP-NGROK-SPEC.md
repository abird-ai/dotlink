# OAuth, remote HTTP and ngrok specification

## Purpose

dotlink embeds OAuth so a single self-hosted owner can expose Streamable HTTP MCP safely to ChatGPT/Claude.ai/other web clients without running a second authentication service.

This is intentionally **not** a general identity provider.

## Core model

```text
remote MCP client
      ↓ HTTPS
ngrok/reverse proxy/direct public HTTP
      ↓
dotlink OAuth resource server
      ↓ bearer validation
MCP Streamable HTTP
      ↓
LocalMachine policy
```

OAuth decides **who may enter**.

LocalMachine decides **what they may do**.

## Public endpoints

```text
/.well-known/oauth-protected-resource
/.well-known/oauth-authorization-server
/oauth/authorize
/oauth/token
/oauth/register
/oauth/revoke
```

MCP:

```text
/mcp
or
/mcp/<ephemeral-token>
```

OAuth routes remain public.

Bearer middleware wraps only the MCP subrouter and executes before rmcp.

## Protected Resource Metadata

Advertises:

- exact resource URL;
- authorization server;
- supported scopes;
- header bearer method.

Unauthenticated MCP returns:

```http
401 Unauthorized
WWW-Authenticate: Bearer resource_metadata="..."
```

## Authorization Server Metadata

Advertises:

- issuer;
- authorize endpoint;
- token endpoint;
- DCR endpoint;
- revocation endpoint;
- authorization code;
- refresh token;
- PKCE S256;
- public-client token auth `none`;
- CIMD support;
- supported scopes.

## Authorization flow

Required:

```text
response_type=code
PKCE S256
client_id
redirect_uri
resource
code_challenge
code_challenge_method=S256
```

Optional:

```text
state
scope
```

Resource must equal the exact MCP resource for this server instance.

## Scopes

```text
mcp:access
offline_access
```

Unknown scopes are rejected.

`mcp:access` is always present.

`offline_access` requires the client to advertise `refresh_token`.

OAuth scopes do not map to local filesystem/shell capabilities.

## Authorization code

Properties:

- 256-bit random material;
- memory-only;
- one use;
- ~60 second lifetime;
- bound to:
  - client ID;
  - issuer;
  - redirect URI;
  - resource;
  - scope;
  - PKCE challenge;
  - whether refresh issuance was approved/supported.

## Access token

Properties:

- opaque random token;
- stored only as hash/key in memory state;
- not persisted;
- ~15 minute lifetime;
- bound to:
  - client;
  - issuer;
  - resource;
  - scope.

Server restart invalidates access tokens.

A persisted refresh grant lets a client recover a new access token.

## Refresh token

Properties:

- opaque random value returned to client;
- plaintext never persisted;
- SHA-256 hash stored;
- ~90 day expiry;
- rotates on use;
- old token removed/replay rejected;
- exact client/issuer/resource binding.

Refresh token is issued only when the client supports the `refresh_token` grant.

## Owner credential

One local owner password.

Stored:

```text
Argon2id hash only
```

Verification:

- re-read latest persisted owner hash;
- bounded semaphore;
- Argon2 in `spawn_blocking`;
- short bounded wait;
- failed-attempt delay;
- no remotely triggerable global lockout.

## Consent

Authorization UI requires owner password.

The page displays:

- client name;
- client ID;
- redirect URI;
- resource;
- scope.

The UI is deliberately tiny and self-contained.

No external resources/analytics/scripts.

## CIMD

Preferred path for clients that publish Client ID Metadata Documents.

Client ID is itself the metadata URL.

Requirements:

- HTTPS;
- non-root path;
- no userinfo;
- no query;
- no fragment.

Fetch safety:

- no redirects;
- short timeout;
- max body;
- DNS resolution;
- all addresses must be public;
- validated addresses pinned for actual connection;
- metadata client_id exact match.

Client metadata must support:

- authorization_code;
- response type code;
- token endpoint auth method `none` (using the plural supported list as authoritative when present).

Clients may advertise additional response types/auth methods; dotlink uses only the compatible intersection.

## DCR

Fallback for clients that still require Dynamic Client Registration.

Supported public client shape:

- redirect URIs;
- authorization_code;
- optional refresh_token;
- response type code;
- token_endpoint_auth_method=none.

Unauthenticated registration is not persisted immediately.

Flow:

```text
POST /oauth/register
→ bounded pending memory registration
→ authorization request
→ owner approves
→ client promoted atomically to persistent approved state
```

Pending DCR lifetime is short (~15 minutes) and bounded.

Approved client count is bounded.

## Persistent state

Default:

```text
$XDG_STATE_HOME/abird/dotlink/oauth.json
```

Named:

```text
oauth.<profile>.json
```

State schema currently:

```text
version 1
```

Contains:

- owner Argon2id hash;
- approved DCR clients;
- hashed refresh grants.

Does not contain:

- access tokens;
- authorization codes;
- pending DCR;
- owner plaintext.

## Cross-process locking

OAuth state may be touched by multiple dotlink processes.

Examples:

- live server;
- setup;
- profile edit;
- `dotlink oauth revoke`.

All durable read-modify-write uses a dedicated private lock file and OS file lock.

Do not replace this with "atomic write only"; atomic replacement prevents torn files but not lost concurrent updates.

## Management CLI

```bash
dotlink oauth status
dotlink oauth status -p work

dotlink oauth clients
dotlink oauth clients -p work

dotlink oauth revoke CLIENT_ID
dotlink oauth revoke -p work CLIENT_ID

dotlink oauth revoke-all
dotlink oauth revoke-all -p work
```

Revoking a client removes:

- approved DCR client registration;
- its persisted refresh grants.

A live server reloads durable state at relevant security boundaries so refresh revocation takes effect without restart.

Existing access tokens remain memory-only until expiry/restart.

For immediate access-token invalidation:

```text
Ctrl-R
or restart dotlink
```

## Public URL/issuer

OAuth server instance is constructed from a canonical public origin and the exact MCP path.

Do not derive issuer from request headers.

Local loopback:

```text
http://127.0.0.1:<port>
```

is acceptable for local test/use.

Non-loopback public origin:

```text
HTTPS required
```

## Direct non-loopback HTTP

Safe path:

```bash
dotlink --http   --http-bind=0.0.0.0:3000   --oauth   --public-url=https://mcp.example.com
```

Unsafe deliberate opt-out:

```bash
dotlink --http   --http-bind=0.0.0.0:3000   --allow-public-no-auth
```

Persisted profile cannot save an unauthenticated non-loopback HTTP listener.

## ngrok

Public ngrok OAuth is automatic.

```bash
dotlink --http --ngrok
```

If an owner credential is absent, configuration/startup must fail rather than expose unauthenticated ingress.

### Stable domain

```bash
--ngrok-domain=my-dotlink.ngrok.app
```

Config stores hostname only.

dotlink asks the ngrok SDK for that domain and verifies the returned hostname matches.

### Stable vs ephemeral OAuth identity

Durable remote connector:

```text
stable hostname
stable /mcp path
```

Automatic hostname and/or ephemeral path:

- supported;
- good for temporary usage;
- identity changes on restart/change;
- remote OAuth client must reconnect/re-authorize.

## Separate ngrok backend

Local HTTP and ngrok do not share the same externally visible listener.

ngrok uses a separate loopback-only backend.

Benefits:

- local/public path policies can differ;
- local/public OAuth policies can differ;
- local token cannot be replayed against public resource;
- public token cannot be replayed against local resource.

Both server instances may share the same OAuth runtime/state store, but grants/tokens are bound to exact issuer/resource.

## Unsafe public no-auth

Only:

```text
--allow-public-no-auth
```

One-run only.

Valid only if endpoint is actually externally reachable.

Purpose:

- explicit expert escape hatch;
- testing/infrastructure cases where external auth exists elsewhere.

Do not add a persisted config field for this.

## Revocation endpoint semantics

Unknown/already-revoked token:

```text
200 OK
```

Persistence failure:

```text
server error
```

Do not falsely report durable revocation if state write failed.

## OAuth request/input limits

Public form/query fields are bounded.

Body layer is bounded.

Metadata response is bounded.

Token lengths are bounded before hashing/lookup.

Keep this in place when adding protocol features.

## Current interoperability target

Designed for current MCP remote-client expectations including:

- ChatGPT remote MCP/CIMD/public PKCE;
- Claude.ai remote MCP/DCR;
- other OAuth 2.1-style Streamable HTTP MCP clients.

Do not assume all clients use the same registration discovery path.

Support CIMD + DCR as complementary compatibility mechanisms.

## Multi-user future

If dotlink becomes:

- hosted;
- multi-owner;
- organization-managed;
- tenant-based;

stop extending the embedded auth server into a general IdP.

Use an established identity provider and retain dotlink as resource server/local policy layer.

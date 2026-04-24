# Infrastructure

## Hosting Pattern
- Self-hosted services at: `[service].app.stefanberggren.se`

## Nextcloud Server
- **Host**: nextcloud.app.stefanberggren.se
- **Version**: 32.0.6
- **Username**: clawdy (stored as `nextcloud_user`)
- **Auth**: App password (stored as `nextcloud_app_password`)

### Nextcloud CalDAV
- **Endpoint**: `https://nextcloud.app.stefanberggren.se/remote.php/dav/`
- **Principal Path**: `/remote.php/dav/principals/users/clawdy/`
- **Access Method**: CalDAV
- **Login Tool**: Browser-based interactive login available

### Nextcloud Calendars
- Contact birthdays
- Clawdy (Stefan Berggren) — writable, shared, owned by nsg
- Leo (Stefan Berggren) — shared, owned by nsg
- Personal (Stefan Berggren) — shared
- *Deleted*: Clawdy & Stefan

## Other Self-Hosted Services
- **Mastodon**: mastodon.app.stefanberggren.se
- **Email Server**: Self-hosted

## Projects
- **Immich Distribution**: Created by user
- **Blogger Helper**: Rust-based tool with UI preview, Monaco editor, AI chat integration
- **Python script**: Enables blogging from phone

## Containerization
- Uses Podman
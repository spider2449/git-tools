# Git Backup

A desktop tool that backs up repositories from GitHub and Gitea accounts to
one or more destinations. Requires Git on `PATH`.

## Usage

1. Add an account (GitHub or Gitea) with an access token.
2. Add one or more destinations and tick the ones to use.
3. Tick repositories, or use Select all, then start the backup.

Run again at any time to update existing backups.

## Access tokens

An account needs a personal access token, generated on the server's website.
SSH and GPG keys cannot be used: the token also calls the server's API, and
Git uses it over HTTPS.

**GitHub**: under Settings > Developer settings > Personal access tokens >
Tokens (classic), generate a token with these scopes:

| Used as | Scopes |
| --- | --- |
| Source | `repo`, plus `read:org` for organization repositories |
| Destination | `repo` |

**Gitea**: under Settings > Applications > Manage Access Tokens, generate a
token with these permissions:

| Used as | `repository` | `user` | `organization` |
| --- | --- | --- | --- |
| Source | Read | Read | Read, to list organization repositories |
| Destination, personal account | Read and write | Read and write | Not needed |
| Destination, organization | Read and write | Read | Read and write |

Creating a repository on Gitea requires write access to `user` or
`organization`, depending on the owner. Without it, the backup fails with
HTTP 403 and `token does not have at least one of required scope(s)`.

Gitea tokens cannot be changed after creation. To add a permission, generate a
new token and paste it into the account's access token field.

## Destinations

- **Local folder**: a bare mirror at `<folder>/<server>/<owner>/<name>.git`.
- **GitHub or Gitea account**: a repository named `<owner>-<name>`. If it does
  not exist it is created automatically under the account or the chosen
  organization.

A destination repository is private or public like its source. If the source's
visibility changes, the next backup changes the destination to match. A
destination that cannot be made private is not pushed to. Gitea's internal
repositories count as private.

A destination repository that already exists is only pushed to if its
description reads `Backup of <source url>`, which is what this tool sets. Any
other repository with that name is left untouched and reported as an error.

## What is copied

Local mirrors keep every ref. Remote destinations receive branches, tags and
notes. Issues, pull request discussions, wikis, releases and Git LFS objects
are not copied.

Branches deleted at the source stay in the backup unless the
"delete branches removed at the source" option is on. Force pushes at the
source do overwrite the backup's branch.

## Security

Tokens are kept in the system credential store, never in the settings file,
in a repository's Git config, or on a command line.

## Development

    cargo test
    cargo run
    cargo build --release

`cargo test demo_servers -- --ignored` starts two local demo servers for
trying the interface by hand. It adds the credentials `demo-source` and
`demo-dest` to the system credential store; remove them afterwards.

# Git Backup

A desktop tool that backs up repositories from GitHub and Gitea accounts to
one or more destinations. Requires Git on `PATH`.

## Usage

1. Add an account (GitHub or Gitea) with an access token.
2. Add one or more destinations and tick the ones to use.
3. Tick repositories, or use Select all, then start the backup.

Run again at any time to update existing backups.

## Destinations

- **Local folder**: a bare mirror at `<folder>/<server>/<owner>/<name>.git`.
- **GitHub or Gitea account**: a repository named `<owner>-<name>`. If it does
  not exist it is created automatically, always private, under the account or
  the chosen organization.

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

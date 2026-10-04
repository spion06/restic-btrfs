# rbtrfs init

Create the repository.

Creates a new restic repository at the profile's `repository`, using its password and `compression`. Fails if one already exists. `backup` does this by itself unless `auto_init = false` or `--no-init` is set.

## Usage

```
rbtrfs init [OPTIONS]
```

## Options

### `--profile <PROFILE>`

Profile to use from the config file.

Default: `default`

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.

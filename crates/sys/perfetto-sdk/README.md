# `perfetto-sdk`

Files in this directory are taken from releases of `perfetto`, using these
rough steps:

```shell
$ git clone https://android.googlesource.com/platform/external/perfetto
$ cd perfetto
$ git checkout v48.1 # Or some other release tag
$ cp sdk/* .../perfetto-sdk/
```

If you update to a new version, please also update the tag specified in
this README. The current version is from release `v48.1`.

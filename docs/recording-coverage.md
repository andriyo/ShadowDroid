# Recording coverage

Every newly written video manifest has a companion `coverage.json`.
`video status` also reports coverage as segments finalize, so a zero-duration
segment can be detected before stopping the full recording.

```sh
shadowdroid video record --out recording-check --duration 5s
shadowdroid video coverage recording-check
shadowdroid video coverage recording-bundle
```

`video coverage <bundle>` is an offline read and also works for older bundles
that have a manifest but no coverage file. It reports:

- The host-observed interval and verified media duration of each segment.
- Unavailable segments and the recorder's declared rollover gaps.
- Concatenated media ranges, plus export ranges only when `video.mp4` exists
  as a completed manifest artifact.
- Each marker's enclosing export segment, or `unavailable`/`not_exported`.

A running process does not prove encoded coverage. Static screens can produce
very few encoded samples, and the investigation had a segment with one sample
and zero duration. This report identifies that loss; it does not claim a
particular Android backend cause or manufacture missing frames.

Host markers and the media clock are not frame-synchronized. Therefore
`export_offset_ms` is null: an enclosing range is supported by the manifest,
but an exact seek position is not. In particular, dropping an unplayable idle
segment must never move its markers into unrelated footage.

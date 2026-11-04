# examples

Model-row templates for SimpleHarn. They are not config. Copy one file
into the `ModelConf` folder beside the exe, then edit it.

Two groups:

- `engine-*.yaml` - one row per engine (`llama`, `strata`, `basalt`,
  `ninfer`). Each row carries every key that engine requires.
- `tab-*.yaml` - one row per category (`WORK`, `RP`, `AUX`, `EMB`), with
  the eviction rule of that category in the header comment.

The file stem is the model identity. Rename the copy to the model name
you want, for example `ModelConf\my-coder.yaml`. Do not set a `name`
key that differs from the stem; the scan refuses it.

Every path and name in these files is a placeholder, except where the
comment marks a documented default (`strata_python` and `strata_cwd`).
Replace the placeholders with real absolute paths before you load. The
scan drops a row whose model file is missing and names the file. Keys
marked optional in the comments may stay omitted.

Required keys per engine (schema: `docs/reference/MODELS.md`):

| Engine | Required keys |
|---|---|
| `llama` | `model` |
| `strata` | `strata_exe`, `serve_model`, `strata_tokenizer` |
| `basalt` | `model` (an absolute `.basalt` pack), `serve_model`, `basalt_python`, `basalt_cwd`, `basalt_exe` |
| `ninfer` | `ninfer_image`, `ninfer_container`, `ninfer_model_name`, `ninfer_artifact`, `ninfer_model_roots` |

An unknown key warns but never drops a row; an engine key the row's
engine never reads reports an inert-key hint. After you edit files,
press Rescan on the page or send `POST /har/rescan`.

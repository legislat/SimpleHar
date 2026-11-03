# Research environment

Last verified: 2026-11-03T19:56:21Z

- OS: Windows 11 x64.
- Harness: SimpleHar SE, one GPU, cardless work only in this session.
- GPU: RTX PRO 5000 Blackwell 72 GB, one card (local install guide). The
  `TASK-BASALT` note says "32GB class"; treat VRAM as a declared row value, not a
  design constant.
- RAM: 512 GB.
- Basalt source: `C:\Inference\Basalt`, local probe at `421063f` (2026-11-03).
- Basalt upstream at investigation time: `9595ae0` (2026-11-03), moving `main`;
  there are no releases or tags to pin.
- Python: `C:\Inference\Basalt\.venv\Scripts\python.exe`, Python 3.14.7.
- Engine binary: `C:\Inference\Basalt\bin\basalt.exe`, 84,435,968 bytes.
- Model packs: no `.basalt` pack is in the SimpleHar workspace. The install guide
  names candidates under `F:\MODELS_MID\basalt\`.
- Existing reference adapter: `src/engine/strata.rs` (template mode, Python server,
  harness-rendered profile).
- Verification limits: no model load and no card run in this session.

Sources:

- `D:\WORKSPACE-VS\installation-guide\basalt\Basalt-INSTALL.md` (local install
  guide, generated 2026-10-25; local probe on 2026-11-03 found `421063f`).
- `https://github.com/jesdga95/basalt` (upstream repository, accessed 2026-11-03).

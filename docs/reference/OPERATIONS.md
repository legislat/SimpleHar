---
title: Operations reference
owner: operator
status: active
audience: [user, contributor, agent]
source_of_truth_for: [autostart, file layout, upgrade, uninstall, backup, log retention]
last_reviewed: 2026-11-03
review_trigger: behavior-change, release
---

# Operations reference

Up: `docs/DOCS_README.md`. This file owns autostart, file layout, upgrade,
uninstall, backup, and log retention.

## Autostart

Replace `<install-dir>` below with the folder that holds
`simplehar-se.exe` and `autostart.vbs`. Create `autostart.vbs` at the path
the `Run` key below names. It runs the harness hidden at logon, once:

```vb
Option Explicit
Dim dir, exe, sh, wmi, procs
dir = "<install-dir>"
exe = dir & "\simplehar-se.exe"
If Not CreateObject("Scripting.FileSystemObject").FileExists(exe) Then WScript.Quit 1
Set sh = CreateObject("WScript.Shell")
Set wmi = GetObject("winmgmts:\\.\root\cimv2")
Set procs = wmi.ExecQuery("Select * from Win32_Process Where Name = 'simplehar-se.exe'")
If procs.Count > 0 Then WScript.Quit 0
sh.CurrentDirectory = dir
sh.Run """" & exe & """", 0, False
```

```powershell
# register / unregister
Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name SimpleHarSE -Value 'wscript.exe "<install-dir>\autostart.vbs"'
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name SimpleHarSE
```

## File layout

The harness keeps its state beside the exe:

| Item | Role |
|---|---|
| `simplehar-se.exe` | The harness itself. |
| `simplehar-se.json` | The config file, beside the exe. `SIMPLEHAR_CONFIG` overrides the path. |
| `ModelConf\` | The catalog directory. Default `<exe dir>\ModelConf`. |
| `logs\` | The harness log and the engine child logs. |
| `.simplehar\strata-profiles\` | The rendered strata profile JSONs, written at each load. |
| `.simplehar\basalt-profiles\` | The rendered basalt profile JSONs, written at each load. |
| `ninfer-claims.json` | The ninfer claim ledger beside the harness exe. |
| `ninfer-autostart-template.sh` | The readable copy of the ninfer launch template, beside the exe in the release package. |
| `autostart.vbs` | Present only when you registered autostart, at the path the `Run` key names. |

## Upgrade

Stop the running harness (the tray menu offers Exit). Back up
`simplehar-se.json` and `ModelConf` first. Then replace `simplehar-se.exe`
with the new exe and start the harness again. Keep the existing
`simplehar-se.json` and `ModelConf`: the config file beside the exe is
authoritative, and the catalog carries over unchanged.

## Uninstall

Stop the app through the tray menu (Exit). Remove the autostart entry with
the `Remove-ItemProperty` command in Autostart above. Then remove the
install folder.

## Backup

Copy `simplehar-se.json` and `ModelConf`; together they are your harness
config and your model catalog. Logs in `logs\` are runtime artifacts and are
disposable.

## Log retention

Logs live in `logs/` next to the exe. The harness keeps **its own**
streaming log, `logs/harness-<boot-epoch>.log`. Each event is one JSON
line, written unbuffered, so a killed process keeps what it logged. The
file is never quieter than `INFO`, whatever `SIMPLEHAR_LOG` asks for, and
it rolls to a new generation at 8 MiB. The last 10 harness log files
survive, rolled generations included. Engine children keep
`logs/<model>-<epoch>.log`. Per model, the newest five launches' client
captures and container snapshots survive; the harness log budget is
separate and can neither consume nor add to that per-model budget.

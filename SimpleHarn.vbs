' SimpleHarn hidden manual launcher.
' Double-click this file to start the harness with no console window.
' The harness folder is the folder that holds this script and simpleharn.exe.
Option Explicit

Dim fso, shell, dir, exe
Set fso = CreateObject("Scripting.FileSystemObject")
Set shell = CreateObject("WScript.Shell")

dir = fso.GetParentFolderName(WScript.ScriptFullName)
exe = fso.BuildPath(dir, "simpleharn.exe")
If Not fso.FileExists(exe) Then
    MsgBox "simpleharn.exe is not beside this launcher.", vbExclamation, "SimpleHarn"
    WScript.Quit 1
End If

shell.CurrentDirectory = dir
shell.Run """" & exe & """", 0, False

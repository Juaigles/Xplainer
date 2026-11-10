# Smoke test of the release executable: both shortcuts registered, a single instance that shows
# the popup when launched again, no autostart entry, shortcuts freed on exit.
# Usage: powershell -ExecutionPolicy Bypass -File scripts/smoke-test.ps1 [-Exe <path to xplanator.exe>]
# Close any running Xplanator (installed or dev) first: they share the shortcuts and the identifier.
param([string]$Exe = "$PSScriptRoot\..\src-tauri\target\release\xplanator.exe")
$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class Smoke {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool RegisterHotKey(IntPtr h, int id, uint mods, uint vk);
  [DllImport("user32.dll")] public static extern bool UnregisterHotKey(IntPtr h, int id);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  // Visibility of the process's window titled "Xplanator" (the popup); null if it has none.
  public static bool? PopupVisible(uint pid) {
    bool? visible = null;
    EnumWindows((h, l) => {
      uint owner; GetWindowThreadProcessId(h, out owner);
      var title = new StringBuilder(64); GetWindowText(h, title, 64);
      if (owner == pid && title.ToString() == "Xplanator") { visible = IsWindowVisible(h); return false; }
      return true;
    }, IntPtr.Zero);
    return visible;
  }
}
"@

$failures = 0
function Check([string]$name, [bool]$ok, [string]$detail = "") {
  $status = if ($ok) { "PASS" } else { "FAIL" }
  Write-Output "$status  $name$(if ($detail) { " ($detail)" })"
  if (-not $ok) { $script:failures++ }
}

# Alt+X and Win+Shift+Z, as the app registers them. Taken means another process holds it (error 1409).
$shortcuts = @(@{ Name = "Alt+X"; Mods = 0x1; Vk = 0x58 }, @{ Name = "Win+Shift+Z"; Mods = 0xC; Vk = 0x5A })
function Test-Free($shortcut) {
  $free = [Smoke]::RegisterHotKey([IntPtr]::Zero, 9000 + $shortcut.Vk, $shortcut.Mods, $shortcut.Vk)
  if ($free) { [void][Smoke]::UnregisterHotKey([IntPtr]::Zero, 9000 + $shortcut.Vk) }
  $free
}

$Exe = (Resolve-Path $Exe).Path
if (Get-Process xplanator, explicador -ErrorAction SilentlyContinue) { throw "Close Xplanator and Explicador before the smoke test." }
foreach ($s in $shortcuts) { Check "$($s.Name) is free before starting" (Test-Free $s) }

$first = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Seconds 5
Check "the app keeps running" (-not $first.HasExited)
foreach ($s in $shortcuts) { Check "$($s.Name) is registered by the app" (-not (Test-Free $s)) }
Check "the popup starts hidden" ([Smoke]::PopupVisible([uint32]$first.Id) -eq $false)

$second = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Seconds 4
$second.Refresh()
Check "a second launch exits by itself" $second.HasExited
Check "only one process remains" (@(Get-Process xplanator -ErrorAction SilentlyContinue).Count -eq 1)
Check "the second launch shows the popup of the first" ([Smoke]::PopupVisible([uint32]$first.Id) -eq $true)

$run = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$entries = @($run.PSObject.Properties | Where-Object { "$($_.Value)" -match "xplanator" -or $_.Name -match "xplanator" })
Check "no 'start with Windows' entry by default" ($entries.Count -eq 0) (($entries | ForEach-Object { $_.Name }) -join ", ")

Stop-Process -Id $first.Id
Start-Sleep -Seconds 2
foreach ($s in $shortcuts) { Check "$($s.Name) is free again after quitting" (Test-Free $s) }

if ($failures) { Write-Output "$failures check(s) failed."; exit 1 }
Write-Output "All checks passed."

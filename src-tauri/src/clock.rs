//! Alarms go into Windows' own Clock app, so they ring like any other alarm there (and are kept there).
//!
//! There's no way in for programs, so ULTRON fills in Clock's own "Add an alarm" form through Windows'
//! UI Automation, the way a screen reader works it: the alarm page, the add button, the hour, minute and
//! AM/PM pickers (each takes a value as it is), the name, the days it repeats on, Save. Then it checks the
//! new alarm is in Clock's list, and closes Clock (its alarms ring with the app closed).

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const NO_WINDOW: u32 = 0x0800_0000;

const SCRIPT: &str = r#"
param([int]$H, [int]$M, [string]$Label, [string]$Days)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$A = [System.Windows.Automation.AutomationElement]; $T = [System.Windows.Automation.TreeScope]; $C = [System.Windows.Automation.Condition]
$VP = [System.Windows.Automation.ValuePattern]::Pattern; $IP = [System.Windows.Automation.InvokePattern]::Pattern
$TP = [System.Windows.Automation.TogglePattern]::Pattern; $SP = [System.Windows.Automation.SelectionItemPattern]::Pattern
function Win() { foreach ($w in $A::RootElement.FindAll($T::Children, $C::TrueCondition)) { if ($w.Current.Name -eq 'Clock' -and $w.Current.ClassName -eq 'ApplicationFrameWindow') { return $w } }; $null }
function Id($el, $id) { $el.FindFirst($T::Descendants, (New-Object System.Windows.Automation.PropertyCondition($A::AutomationIdProperty, $id))) }
function Named($el, $n) { $el.FindFirst($T::Descendants, (New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, $n))) }
# (PowerShell's names ignore case, and a function sees its caller's: nothing here may be called $t, $a or $c)
function WaitFor($sb, $ms) { $sw = [Diagnostics.Stopwatch]::StartNew(); while ($sw.ElapsedMilliseconds -lt $ms) { $r = & $sb; if ($r) { return $r }; Start-Sleep -Milliseconds 150 }; $null }
function Done($s) { Write-Output $s; exit }
Start-Process 'ms-clock:'
$w = WaitFor { Win } 12000
if (-not $w) { Done 'fail|Clock did not open' }
$nav = WaitFor { Id $w 'AlarmButton' } 8000
if (-not $nav) { Done 'fail|no alarm page' }
try { $nav.GetCurrentPattern($SP).Select() } catch { try { $nav.GetCurrentPattern($IP).Invoke() } catch {} }
$add = WaitFor { Id $w 'AddAlarmButton' } 6000
if (-not $add) { Done 'fail|no add button' }
$add.GetCurrentPattern($IP).Invoke()
$hour = WaitFor { Id $w 'HourPicker' } 6000
if (-not $hour) { Done 'fail|no form' }
$min = Id $w 'MinutePicker'; $per = Id $w 'TwelveHourPicker'; $all = Id $w 'DurationPicker'
if ($per) {
  $h12 = $H % 12; if ($h12 -eq 0) { $h12 = 12 }
  $hour.GetCurrentPattern($VP).SetValue(('{0:D2}' -f $h12))
  $min.GetCurrentPattern($VP).SetValue(('{0:D2}' -f $M))
  $per.GetCurrentPattern($VP).SetValue($(if ($H -lt 12) { 'AM' } else { 'PM' }))
} else {
  $hour.GetCurrentPattern($VP).SetValue(('{0:D2}' -f $H))
  $min.GetCurrentPattern($VP).SetValue(('{0:D2}' -f $M))
}
Start-Sleep -Milliseconds 300
$want = '{0:D2}:{1:D2}' -f $H, $M
$got = try { $all.GetCurrentPattern($VP).Current.Value } catch { '' }
if ($all -and $got -notmatch [regex]::Escape($want)) { try { (Id $w 'CloseButton').GetCurrentPattern($IP).Invoke() } catch {}; Done "fail|time did not take ($got)" }
$name = Named $w 'Alarm name'
if ($name -and $Label) { $name.GetCurrentPattern($VP).SetValue($Label) }
if ($Days -and $Days -ne 'once') {
  $rep = Id $w 'RepeatCheckBox'
  if ($rep -and $rep.GetCurrentPattern($TP).Current.ToggleState -ne 'On') { $rep.GetCurrentPattern($TP).Toggle(); Start-Sleep -Milliseconds 300 }
  $on = switch ($Days) { 'weekdays' { 'Monday','Tuesday','Wednesday','Thursday','Friday' } 'weekends' { 'Saturday','Sunday' } default { 'Sunday','Monday','Tuesday','Wednesday','Thursday','Friday','Saturday' } }
  foreach ($d in 'Sunday','Monday','Tuesday','Wednesday','Thursday','Friday','Saturday') {
    $b = Named $w $d; if (-not $b) { continue }
    try { $tp = $b.GetCurrentPattern($TP); $isOn = $tp.Current.ToggleState -eq 'On'; if ($isOn -ne ($on -contains $d)) { $tp.Toggle() } } catch {}
  }
}
(Id $w 'PrimaryButton').GetCurrentPattern($IP).Invoke()
Start-Sleep -Milliseconds 900
$list = Id $w 'AlarmsScrollViewer'
$seen = $false
if ($list) { foreach ($i in $list.FindAll($T::Children, $C::TrueCondition)) { if ($i.Current.Name -like "*$Label*") { $seen = $true } } }
try { $w.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern).Close() } catch {}
if ($seen) { Done "ok|$want" } else { Done "ok?|$want" }
"#;

/// Add an alarm to Windows' Clock at `hour`:`minute` (24-hour), named `label`, repeating on `days`
/// ("once", "daily", "weekdays" or "weekends"; an empty argument would be lost on the way to PowerShell). Gives back what Clock now has, or why it couldn't.
pub fn add_alarm(hour: u32, minute: u32, label: &str, days: &str) -> Result<String, String> {
    let file = std::env::temp_dir().join(format!("ultron-clock-{}.ps1", std::process::id()));
    std::fs::write(&file, SCRIPT).map_err(|e| e.to_string())?;
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&file)
        .args(["-H", &hour.to_string(), "-M", &minute.to_string(), "-Label", if label.is_empty() { "ULTRON" } else { label }, "-Days", if days.is_empty() { "once" } else { days }])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if t0.elapsed() > Duration::from_secs(40) {
            let _ = child.kill();
            let _ = std::fs::remove_file(&file);
            return Err("Clock didn't answer in time".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&file);
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    crate::log_line(&format!("clock: {} (took {} ms)", text.lines().last().unwrap_or(""), t0.elapsed().as_millis()));
    match text.lines().last().unwrap_or("").split_once('|') {
        Some(("ok", t)) | Some(("ok?", t)) => Ok(t.to_string()),
        Some((_, why)) => Err(why.to_string()),
        None => Err(String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("Clock couldn't be worked").to_string()),
    }
}

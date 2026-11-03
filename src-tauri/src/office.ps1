# SYLVER's Office hands: builds Excel workbooks and Word documents, and reads them back, through
# Office's own automation (no clicking, nothing guessed from pixels). One JSON request on stdin,
# one JSON answer on stdout.
param([string]$Mode)
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Text.Encoding]::UTF8
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$req = [Console]::In.ReadToEnd() | ConvertFrom-Json

function Done($o) { $o | ConvertTo-Json -Compress -Depth 5; exit 0 }
function Fail([string]$why) { @{ ok = $false; error = $why } | ConvertTo-Json -Compress; exit 0 }

# "1,299", "Rs 1,299", "$12.50", "45%" -> numbers Excel can add up and chart; anything else stays text
$money = '[\s,' + [char]0x20B9 + '$' + [char]0x20AC + [char]0x00A3 + ']|^rs\.?|^inr'
function Cell($v) {
  if ($null -eq $v) { return @('', $null) }
  if ($v -is [double] -or $v -is [int] -or $v -is [long] -or $v -is [decimal]) { return @([double]$v, $null) }
  $s = ([string]$v).Trim()
  if ($s.StartsWith('=')) { return @($s, $null) }
  $c = ($s.ToLower() -replace $money, '')
  if ($c -match '^-?\d+(\.\d+)?%$') { return @(([double]$c.TrimEnd('%')) / 100, '0%') }
  if ($c -match '^-?\d+(\.\d+)?$' -and $c.Length -lt 16) {
    $fmt = $null
    if ($s -match [char]0x20B9 -or $s.ToLower().StartsWith('rs') -or $s.ToLower().StartsWith('inr')) { $fmt = ([char]0x20B9) + ' #,##0' }
    elseif ($s.StartsWith('$')) { $fmt = '$#,##0.00' }
    elseif ($s -match ',') { $fmt = '#,##0' }
    return @([double]$c, $fmt)
  }
  return @($s, $null)
}

if ($Mode -eq 'excel') {
  $cols = @($req.columns); $rows = @($req.rows)
  if ($cols.Count -eq 0) { Fail 'The table needs at least one column.' }
  $xl = New-Object -ComObject Excel.Application
  $xl.DisplayAlerts = $false
  $xl.Visible = $false
  try {
    $wb = $xl.Workbooks.Add()
    $ws = $wb.Worksheets.Item(1)
    $name = ([string]$req.title -replace '[\\/\?\*\[\]:]', ' ').Trim()
    if ($name.Length -gt 31) { $name = $name.Substring(0, 31) }
    if ($name) { try { $ws.Name = $name } catch {} }
    $ws.Cells.Item(1, 1).Value2 = [string]$req.title
    $ws.Cells.Item(1, 1).Font.Size = 16
    $ws.Cells.Item(1, 1).Font.Bold = $true
    $nr = $rows.Count + 1; $nc = $cols.Count; $top = 3
    $data = New-Object 'object[,]' $nr, $nc
    $fmts = @{}
    $numeric = @{}
    for ($c = 0; $c -lt $nc; $c++) { $data[0, $c] = [string]$cols[$c]; $numeric[$c] = $rows.Count -gt 0 }
    for ($r = 0; $r -lt $rows.Count; $r++) {
      $row = @($rows[$r])
      for ($c = 0; $c -lt $nc; $c++) {
        $v = if ($c -lt $row.Count) { $row[$c] } else { '' }
        $cv = Cell $v
        $data[($r + 1), $c] = $cv[0]
        if ($cv[1]) { $fmts[$c] = $cv[1] }
        if (-not ($cv[0] -is [double])) { if ([string]$cv[0] -ne '') { $numeric[$c] = $false } }
      }
    }
    $rng = $ws.Range($ws.Cells.Item($top, 1), $ws.Cells.Item($top + $nr - 1, $nc))
    $rng.Value2 = $data
    foreach ($c in $fmts.Keys) { if ($numeric[$c] -and $nr -gt 1) { $ws.Range($ws.Cells.Item($top + 1, $c + 1), $ws.Cells.Item($top + $nr - 1, $c + 1)).NumberFormat = $fmts[$c] } }
    $lo = $ws.ListObjects.Add(1, $rng, $null, 1)
    $lo.TableStyle = 'TableStyleMedium2'
    if ($req.totals -and $nr -gt 1) {
      $lo.ShowTotals = $true
      for ($c = 1; $c -lt $nc; $c++) { if ($numeric[$c]) { $lo.ListColumns.Item($c + 1).TotalsCalculation = 1 } }
    }
    [void]$ws.Columns.AutoFit()
    for ($c = 1; $c -le $nc; $c++) { if ($ws.Columns.Item($c).ColumnWidth -gt 60) { $ws.Columns.Item($c).ColumnWidth = 60; $ws.Columns.Item($c).WrapText = $true } }
    $types = @{ column = 51; bar = 57; line = 4; pie = 5 }
    $kind = [string]$req.chart
    $series = @(); for ($c = 1; $c -lt $nc; $c++) { if ($numeric[$c]) { $series += $c } }
    if ($types.ContainsKey($kind) -and $nr -gt 1 -and $series.Count -gt 0) {
      # the labels plus every numeric column, as one multi-area address ("A3:A6,C3:C6")
      $col = { param($i) $s = ''; $n = $i; while ($n -gt 0) { $m = ($n - 1) % 26; $s = [char](65 + $m) + $s; $n = [Math]::Floor(($n - 1) / 26) }; $s }
      $areas = @(1) + ($series | ForEach-Object { $_ + 1 }) | ForEach-Object { (& $col $_) + $top + ':' + (& $col $_) + ($top + $nr - 1) }
      $src = $ws.Range(($areas -join ','))
      $left = $ws.Cells.Item($top, $nc + 2).Left
      $shape = $ws.Shapes.AddChart2(-1, $types[$kind], $left, $ws.Cells.Item($top, 1).Top, 520, 320)
      $shape.Chart.SetSourceData($src)
      $shape.Chart.HasTitle = $true
      $shape.Chart.ChartTitle.Text = [string]$req.title
    }
    $ws.Cells.Item(1, 1).Select() | Out-Null
    $out = [string]$req.out
    New-Item -ItemType Directory -Force -Path (Split-Path $out) | Out-Null
    $wb.SaveAs($out, 51)
    if ($req.open) { $xl.Visible = $true; $xl.WindowState = -4137; $xl.UserControl = $true }
    else { $wb.Close($false); $xl.Quit() }
    Done @{ ok = $true; path = $out; rows = $rows.Count; columns = $nc; chart = ($types.ContainsKey($kind) -and $series.Count -gt 0) }
  } catch {
    $why = $_.Exception.Message
    try { $xl.DisplayAlerts = $false; foreach ($w in @($xl.Workbooks)) { $w.Close($false) }; $xl.Quit() } catch {}
    Fail $why
  }
}

if ($Mode -eq 'excel_read') {
  $xl = New-Object -ComObject Excel.Application
  $xl.DisplayAlerts = $false
  $xl.Visible = $false
  try {
    $wb = $xl.Workbooks.Open([string]$req.path, 0, $true)
    $lines = New-Object System.Collections.Generic.List[string]
    foreach ($ws in $wb.Worksheets) {
      $ur = $ws.UsedRange
      $nr = $ur.Rows.Count; $nc = $ur.Columns.Count
      $lines.Add("Sheet: " + $ws.Name + " (" + $nr + " rows x " + $nc + " columns)")
      $v = $ur.Value2
      if ($nr -eq 1 -and $nc -eq 1) { $lines.Add([string]$v) }
      else {
        for ($r = 1; $r -le [Math]::Min($nr, 60); $r++) {
          $cells = @(); for ($c = 1; $c -le [Math]::Min($nc, 15); $c++) { $cells += [string]$v[$r, $c] }
          $lines.Add(($cells -join ' | '))
        }
        if ($nr -gt 60) { $lines.Add("... " + ($nr - 60) + " more rows") }
      }
      if ($lines.Count -gt 160) { break }
    }
    $wb.Close($false); $xl.Quit()
    Done @{ ok = $true; text = ($lines -join "`n") }
  } catch {
    $why = $_.Exception.Message
    try { $xl.Quit() } catch {}
    Fail $why
  }
}

if ($Mode -eq 'word') {
  $wd = New-Object -ComObject Word.Application
  $wd.Visible = $false
  $wd.DisplayAlerts = 0
  try {
    $doc = $wd.Documents.Add()
    $sel = $wd.Selection
    function Put([string]$text, [int]$style) {
      $sel.Style = $doc.Styles.Item([ref]$style)
      $sel.TypeText($text)
      $sel.TypeParagraph()
    }
    Put ([string]$req.title) -63                              # Title
    foreach ($b in @($req.sections)) {
      if ($b.heading) { Put ([string]$b.heading) -2 }        # Heading 1
      foreach ($line in ([string]$b.text -split "`r?`n")) {
        $t = $line.Trim()
        if ($t -eq '') { continue }
        if ($t -match '^[-*•]\s+(.*)$') { Put $matches[1] -49 }   # List Bullet
        else { Put $t -1 }                                     # Normal
      }
    }
    $out = [string]$req.out
    New-Item -ItemType Directory -Force -Path (Split-Path $out) | Out-Null
    $doc.SaveAs2([ref]$out, [ref]16)
    if ($req.open) { $wd.Visible = $true; $doc.Activate(); $sel.HomeKey(6) | Out-Null; $wd.Activate() }
    else { $doc.Close([ref]0); $wd.Quit([ref]0) }
    Done @{ ok = $true; path = $out; sections = @($req.sections).Count }
  } catch {
    $why = $_.Exception.Message
    try { foreach ($d in @($wd.Documents)) { $d.Close([ref]0) }; $wd.Quit([ref]0) } catch {}
    Fail $why
  }
}

if ($Mode -eq 'word_read') {
  $wd = New-Object -ComObject Word.Application
  $wd.Visible = $false
  $wd.DisplayAlerts = 0
  try {
    # Word reads PDFs too (it converts them quietly)
    $path = [string]$req.path
    $doc = $wd.Documents.Open([ref]$path, [ref]$false, [ref]$true, [ref]$false)
    $text = $doc.Content.Text
    $doc.Close([ref]0); $wd.Quit([ref]0)
    if ($text.Length -gt 12000) { $text = $text.Substring(0, 12000) }
    Done @{ ok = $true; text = $text }
  } catch {
    $why = $_.Exception.Message
    try { $wd.Quit([ref]0) } catch {}
    Fail $why
  }
}

Fail ('Unknown mode ' + $Mode)

# ULTRON's deck builder: designs a PowerPoint deck live in front of you while the brain is
# still writing it. Slides arrive one per line on stdin (JSON): {"k":"title"...}, then
# {"k":"slide"...} as each is written, then {"k":"end"}. Morph transitions between slides
# (the glowing shapes glide), titles fade in, an accent line wipes across, points appear one by one.
param([string]$Out)
$ErrorActionPreference = 'Continue'
[Console]::InputEncoding = [Text.Encoding]::UTF8

function Rgb([string]$hex) { $h = $hex.TrimStart('#'); [int]("0x" + $h.Substring(4, 2)) * 65536 + [int]("0x" + $h.Substring(2, 2)) * 256 + [int]("0x" + $h.Substring(0, 2)) }
function Safe([scriptblock]$b) { try { & $b } catch { } }

$themes = @(
  @{ bg1 = '#0B1020'; bg2 = '#1C2452'; acc = '#7C9CFF'; acc2 = '#4FE3C1'; text = '#FFFFFF'; sub = '#B8C2E0' },
  @{ bg1 = '#140B08'; bg2 = '#3A1C12'; acc = '#FF8A4C'; acc2 = '#FFD166'; text = '#FFF7F0'; sub = '#E8C9B8' },
  @{ bg1 = '#07150F'; bg2 = '#123628'; acc = '#4ADE80'; acc2 = '#A7F3D0'; text = '#F2FFF8'; sub = '#B5D9C6' },
  @{ bg1 = '#120C1F'; bg2 = '#2E1A4A'; acc = '#C084FC'; acc2 = '#F472B6'; text = '#FBF5FF'; sub = '#D3C3E6' },
  @{ bg1 = '#05121C'; bg2 = '#0C2D48'; acc = '#38BDF8'; acc2 = '#A5F3FC'; text = '#F0FAFF'; sub = '#B7D3E6' }
)
$t = $themes[0]
$TitleFont = 'Segoe UI Semibold'; $BodyFont = 'Segoe UI'

$app = New-Object -ComObject PowerPoint.Application
$app.Visible = -1
$pres = $app.Presentations.Add(-1)
Safe { $pres.PageSetup.SlideWidth = 960; $pres.PageSetup.SlideHeight = 540 }
Safe { $app.WindowState = 1 }   # maximized, so you can watch it come together

# where the two glowing orbs sit on each slide: Morph glides them between slides
$orbs = @(
  @(640, -120, 520, 520, -140, 300, 380, 380),
  @(700, 260, 460, 460, -160, -160, 420, 420),
  @(-120, 280, 480, 480, 640, -140, 440, 440),
  @(560, -200, 560, 560, 40, 330, 320, 320),
  @(-160, -180, 500, 500, 660, 250, 400, 400)
)

function NewSlide([int]$i) {
  $s = $pres.Slides.Add($i, 12)
  Safe {
    $s.FollowMasterBackground = 0
    $f = $s.Background.Fill
    $f.TwoColorGradient(4, 1)
    $f.ForeColor.RGB = (Rgb $t.bg1)
    $f.BackColor.RGB = (Rgb $t.bg2)
  }
  $o = $orbs[($i - 1) % $orbs.Count]
  foreach ($k in 0, 1) {
    Safe {
      $sh = $s.Shapes.AddShape(9, $o[$k * 4], $o[$k * 4 + 1], $o[$k * 4 + 2], $o[$k * 4 + 3])
      $sh.Name = "!!orb$k"
      $sh.Line.Visible = 0
      $sh.Fill.ForeColor.RGB = (Rgb ($(if ($k -eq 0) { $t.acc } else { $t.acc2 })))
      $sh.Fill.Transparency = $(if ($k -eq 0) { 0.8 } else { 0.86 })
      Safe { $sh.SoftEdge.Radius = 90 }
    }
  }
  Safe { $s.SlideShowTransition.EntryEffect = 3953 }       # Morph
  Safe { if ($s.SlideShowTransition.EntryEffect -ne 3953) { $s.SlideShowTransition.EntryEffect = 3849 } }   # or a smooth fade
  Safe { $s.SlideShowTransition.Duration = 1.1 }
  return $s
}

function Text($s, [string]$txt, $x, $y, $w, $h, [int]$size, [string]$color, [string]$font, [string]$name) {
  $tb = $s.Shapes.AddTextbox(1, $x, $y, $w, $h)
  Safe { $tb.Name = $name }
  $r = $tb.TextFrame.TextRange
  $r.Text = $txt
  $r.Font.Name = $font; $r.Font.Size = $size; $r.Font.Color.RGB = (Rgb $color)
  Safe { $tb.TextFrame.WordWrap = -1; $tb.TextFrame.AutoSize = 0; $tb.Height = $h }
  return $tb
}

function Animate($s, $shape, [int]$effect, [int]$level, [int]$trigger, [double]$dur, [double]$delay) {
  Safe {
    $seq = $s.TimeLine.MainSequence
    $before = $seq.Count
    $e = $seq.AddEffect($shape, $effect, $level, $trigger)
    for ($k = $before + 1; $k -le $seq.Count; $k++) {
      $it = $seq.Item($k)
      Safe { $it.Timing.Duration = $dur }
      if ($k -gt $before + 1) { Safe { $it.Timing.TriggerType = 3; $it.Timing.TriggerDelayTime = $delay } }
    }
    return $e
  }
}

function Bar($s, $x, $y) {
  $b = $s.Shapes.AddShape(5, $x, $y, 64, 5)
  Safe { $b.Name = '!!bar'; $b.Line.Visible = 0; $b.Fill.ForeColor.RGB = (Rgb $t.acc) }
  return $b
}

$n = 8; $i = 0
function TitleSlide($m) {
  $script:t = $themes[[int]$m.theme % $themes.Count]
  $script:n = [int]$m.n + 2
  $script:i = 1
  $s = NewSlide 1
  $title = Text $s $m.title 70 160 820 80 54 $t.text $TitleFont '!!title'
  Safe { $title.TextFrame.TextRange.ParagraphFormat.SpaceWithin = 0.9; $title.TextFrame.AutoSize = 1 }
  $below = 160 + [Math]::Max(80, $title.Height) + 14   # the line and subtitle sit right under the title, however many lines it takes
  $bar = Bar $s 74 $below
  $sub = Text $s $m.subtitle 72 ($below + 18) 760 60 22 $t.sub $BodyFont 'sub'
  $tag = Text $s ("Made with ULTRON  ·  " + (Get-Date -Format 'd MMMM yyyy')) 72 470 600 30 12 $t.sub $BodyFont 'tag'
  Animate $s $title 10 0 3 0.8 0 | Out-Null
  Animate $s $bar 22 0 3 0.5 0 | Out-Null
  Animate $s $sub 10 0 2 0.7 0 | Out-Null
  Safe { $app.ActiveWindow.View.GotoSlide(1) }
  Write-Output "SLIDE 1 $n"
}

function ContentSlide($sl) {
  $script:i++
  $i = $script:i
  $s = NewSlide $i
  $num = Text $s ('{0:D2}' -f ($i - 1)) 700 18 210 150 110 $t.acc $TitleFont '!!num'
  Safe { $num.TextFrame2.TextRange.Font.Fill.Transparency = 0.78; $num.TextFrame.TextRange.ParagraphFormat.Alignment = 3 }
  $ttl = Text $s $sl.title 70 52 700 70 34 $t.text $TitleFont '!!title'
  $bar = Bar $s 74 122
  $body = ($sl.bullets | ForEach-Object { $_ }) -join "`r"
  $tb = Text $s $body 72 146 760 340 24 $t.text $BodyFont 'body'
  Safe { $tb.TextFrame.VerticalAnchor = 3 }   # points sit in the middle of the space, not stuck to the top
  Safe {
    $r = $tb.TextFrame.TextRange
    $r.ParagraphFormat.SpaceAfter = 18
    $r.ParagraphFormat.Bullet.Visible = -1
    $r.ParagraphFormat.Bullet.Character = 8226
    $r.ParagraphFormat.Bullet.Font.Color.RGB = (Rgb $t.acc)
    $tb.TextFrame.Ruler.Levels(1).FirstMargin = 0
    $tb.TextFrame.Ruler.Levels(1).LeftMargin = 24
  }
  Animate $s $ttl 10 0 3 0.6 0 | Out-Null
  Animate $s $bar 22 0 2 0.5 0 | Out-Null
  Animate $s $tb 10 2 3 0.5 0.12 | Out-Null
  if ($sl.note) { Safe { $s.NotesPage.Shapes.Placeholders(2).TextFrame.TextRange.Text = $sl.note } }
  Safe { $app.ActiveWindow.View.GotoSlide($i) }
  Write-Output "SLIDE $i $n"
}

function ClosingSlide() {
  $script:i++
  $i = $script:i
  $s = NewSlide $i
  $end = Text $s 'Thank you' 70 180 820 100 60 $t.text $TitleFont '!!title'
  $bar = Bar $s 74 290
  $q = Text $s 'Questions?' 72 306 600 50 24 $t.sub $BodyFont 'sub'
  Animate $s $end 10 0 3 0.8 0 | Out-Null
  Animate $s $bar 22 0 3 0.5 0 | Out-Null
  Animate $s $q 10 0 2 0.7 0 | Out-Null
  Safe { $app.ActiveWindow.View.GotoSlide($i) }
  Write-Output "SLIDE $i $n"
}

Write-Output "READY"
while ($null -ne ($line = [Console]::In.ReadLine())) {
  if (-not $line.Trim()) { continue }
  $m = $line | ConvertFrom-Json
  switch ($m.k) {
    'title' { TitleSlide $m }
    'slide' { ContentSlide $m }
    'end' {
      if ($script:i -ge 2) { ClosingSlide }
      Safe { $app.ActiveWindow.View.GotoSlide(1) }
      $dest = $(if ($m.out) { [string]$m.out } else { $Out })
      $pres.SaveAs($dest)
      Write-Output "DONE $dest"
      exit 0
    }
  }
}

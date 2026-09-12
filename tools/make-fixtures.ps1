# Generates real JPEGs for the end-to-end gallery test.
#
# Why real JPEGs and not the .bin fixtures the harness normally uses: the receiver
# inserts into MediaStore.Images (app_info.md §10.4), and the gallery only shows a
# row it can decode. A .bin with mime application/octet-stream proves the transfer
# but proves nothing about §16 or about the asset being visible, which is the one
# claim that still needs evidence on a real device.
#
# Each file gets a distinct LastWriteTime. The harness maps that to
# AssetDescriptor.created_at, the receiver writes it to DATE_TAKEN, and the gallery
# should then sort these into their real years rather than into today.

param(
    [string]$OutDir = "devdata\src"
)

Add-Type -AssemblyName System.Drawing

$null = New-Item -ItemType Directory -Force -Path $OutDir

$specs = @(
    @{ Name = "psync-red.jpg";   Colour = "Firebrick";  Label = "PhotoSync 001"; Date = "2019-07-04 10:15:00"; W = 1600; H = 1200 },
    @{ Name = "psync-green.jpg"; Colour = "SeaGreen";   Label = "PhotoSync 002"; Date = "2021-12-25 08:30:00"; W = 2048; H = 1536 },
    @{ Name = "psync-blue.jpg";  Colour = "SteelBlue";  Label = "PhotoSync 003"; Date = "2023-03-15 19:45:00"; W = 1280; H = 960 }
)

foreach ($s in $specs) {
    $path = Join-Path (Resolve-Path $OutDir) $s.Name

    $bmp = New-Object System.Drawing.Bitmap($s.W, $s.H)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromName($s.Colour))

    # Some drawn detail, so the three files do not compress to near-identical
    # sizes and so a thumbnail is visibly different from its neighbours.
    $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::White, 6)
    for ($i = 0; $i -lt $s.W; $i += 120) {
        $g.DrawLine($pen, $i, 0, 0, $s.H - $i)
    }

    $font = New-Object System.Drawing.Font("Segoe UI", 72, [System.Drawing.FontStyle]::Bold)
    $brush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)
    $g.DrawString($s.Label, $font, $brush, 60, 60)
    $g.DrawString($s.Date, $font, $brush, 60, 180)

    $g.Dispose()
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Jpeg)
    $bmp.Dispose()

    $stamp = [datetime]::ParseExact($s.Date, "yyyy-MM-dd HH:mm:ss", $null)
    (Get-Item $path).LastWriteTime = $stamp
    (Get-Item $path).CreationTime = $stamp

    $len = (Get-Item $path).Length
    $hash = (Get-FileHash $path -Algorithm SHA256).Hash.ToLower()
    "{0}  {1} bytes  {2}  sha256={3}" -f $s.Name, $len, $stamp.ToString("s"), $hash
}

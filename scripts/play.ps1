# Starts the title with the settings this fork is played with (README.md, "How to run it").
#   scripts\play.ps1 -Exe <path of your own copy's executable>
# -Safe         without the newer, faster ways (the first thing to try when something is wrong)
# -RayTracing   the title's own ray-traced indirect light: lit interiors, about a third slower
# -Build        target\diag (the default, the build all measurements are of) or target\release
# -More         other settings: -More @{VICTORY_PANEL='1'}
# -Console      series-x (the default) or series-s: the console model the title sees (less memory, lower resolution)
# -LowMemory    the card reads the game's memory where it is instead of from copies: about 1 GB less RAM, for PCs whose
#               graphics share their memory (integrated). On a card of its own it reads over PCIe: no slower on Windows
#               with a 7900 XTX, 2.5 times slower on Linux/Proton (RADV) with one.
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$Safe,
    [switch]$RayTracing,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [hashtable]$More = @{},
    [ValidateSet('series-x', 'series-s')][string]$Console = 'series-x',
    [switch]$LowMemory
)
$settings = @{
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = '1'
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0.8'; VICTORY_D3D12_DRED = 'light'
}
if ($Safe) {
    $settings.VICTORY_UNDER_WIDE = '0'; $settings.VICTORY_LAND_ECHO = '0'
} else {
    $settings.VICTORY_THIN = '1'; $settings.VICTORY_JOIN_PARTS = '8'; $settings.VICTORY_UPLOAD_AHEAD = '1'
}
if ($LowMemory) {
    # Buffers and windows the GPU reads where the game has them, windows cut on 64 KiB so that they fall between the
    # game's unmapped pages, also in its private memory, and memory let go under pressure. Windows, 7900 XTX, Series X,
    # at the boat (run xtx-newdefaults): copies of game memory 2.0 GB, were 3.3; RAM 19.4 GB, was 20.4; 50-51 ms a frame.
    # Linux/Proton (RADV), 7900 XTX: 132.7 ms a frame, was 52.0 (PR #206).
    $settings.VICTORY_PORT_ALL = '1'; $settings.VICTORY_WINDOW_KB = '64'; $settings.VICTORY_INPLACE = '1'; $settings.VICTORY_RESIDENCY = '1'
    # Texture blocks of 16 MiB, 4 made ahead, and windows asked for without a length cut on 64 KiB too; with the residency
    # switch the pool's blocks under three quarters used are drained and the textures slots hold moved out on the card
    # (pool.rs `drain`), and staging blocks are 16 MiB. Series S at the boat: RAM 15.9 GB + card 7.8 GB = 23.7 GB,
    # 39.9 ms a frame (run shirt-j; was 16.2 + 11.7 = 27.9 GB, 41.3 ms without these, run shirt-a).
    $settings.VICTORY_TEXTURE_BLOCK_MB = '16'; $settings.VICTORY_RIPE_BLOCKS = '4'; $settings.VICTORY_WINDOW_TAILS = '1'
}
if ($RayTracing) { $settings.VICTORY_BVH_BUILDER = '1' }
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }

$victory = Join-Path $PSScriptRoot "..\target\$Build\victory.exe"
if (-not (Test-Path $victory)) { Write-Host "No $victory. Build it first (README.md)."; exit 1 }
if (-not (Test-Path $Exe)) { Write-Host "No $Exe."; exit 1 }
# (Other settings left over in this window go; where your files are stays.)
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
$inherited = @(Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $paths })
foreach ($entry in $inherited) { Remove-Item "Env:$($entry.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }
$discarded = $inherited | Where-Object {
    $_.Name -notin $More.Keys -and -not $_.Value.Equals([Environment]::GetEnvironmentVariable($_.Name))
} | Sort-Object Name
if ($discarded) {
    Write-Host "Discarded inherited values for $($discarded.Name -join ', '). Use -More to pass them explicitly."
}
$Exe = (Resolve-Path $Exe).Path
Push-Location (Join-Path $PSScriptRoot '..')   # victory.toml is looked for here
try { & $victory run $Exe --execute --console $Console } finally { Pop-Location }

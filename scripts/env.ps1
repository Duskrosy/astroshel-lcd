# Build environment for astroshel-lcd.
# `cargo` and the MinGW-w64 gcc/g++ are NOT on the default PATH (installs happened
# after the shell's parent process started). Dot-source this before any cargo command:
#     . .\scripts\env.ps1 ; cargo build
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
$mingwBin = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.MSVCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
$env:PATH = "$cargoBin;$mingwBin;$env:PATH"

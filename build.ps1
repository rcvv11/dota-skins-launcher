# Builds Dota Skins Launcher with one command:
#   powershell -ExecutionPolicy Bypass -File build.ps1
# Result: the dist\ folder with the launcher, gencatalog.exe and changer.dll.

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

function Need($cmd, $hint) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        Write-Host "Не найдено: $cmd" -ForegroundColor Red
        Write-Host $hint
        exit 1
    }
}

Need "cargo" "Установите Rust: https://rustup.rs (при установке выберите вариант по умолчанию), затем выполните:`n  rustup default stable-x86_64-pc-windows-gnu"
Need "g++" "Установите компилятор C++ MinGW-w64: скачайте w64devkit (https://github.com/skeeto/w64devkit/releases),`nраспакуйте и добавьте его папку bin в переменную PATH."

Write-Host "1/3 Собираю лаунчер..." -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "2/3 Собираю changer.dll..." -ForegroundColor Cyan
g++ -O2 -shared -static -o native\changer.dll native\changer.cpp -lpsapi
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "3/3 Складываю всё в dist\..." -ForegroundColor Cyan
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\dota-skins-launcher.exe dist\ -Force
Copy-Item target\release\gencatalog.exe dist\ -Force
Copy-Item native\changer.dll dist\ -Force

Write-Host ""
Write-Host "Готово: dist\dota-skins-launcher.exe" -ForegroundColor Green
Write-Host "При первом запуске лаунчер сам скачает каталог предметов и Source2Viewer-CLI."

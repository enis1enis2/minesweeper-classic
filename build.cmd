@echo off
rem build.cmd - builds both 32-bit and 64-bit releases of Minesweeper (Classic)
rem Requires MSYS2 with mingw-w64 gcc toolchains. MSYS2_ROOT comes from the
rem environment (CI sets it via setup-msys2) or from tools\msys2_root.cmd.

call "%~dp0tools\msys2_root.cmd"
if not "%MSYS2_ROOT%"=="" goto :have_msys
echo [ERROR] MSYS2_ROOT is not set and no default MSYS2 install was found.
echo Set MSYS2_ROOT to your MSYS2 root, then re-run build.cmd.
echo See tools\msys2_root.cmd for the resolution rules.
exit /b 1
:have_msys

set ROOT=%~dp0
set BUILD=%ROOT%build
set SRC=%ROOT%src
if not exist "%BUILD%" mkdir "%BUILD%"

rem ---------------- x64 ----------------
set PATH=%MSYS2_ROOT%\mingw64\bin;%PATH%
pushd "%SRC%"
"%MSYS2_ROOT%\mingw64\bin\windres.exe" resources.rc -o "%BUILD%\resources-x64.o" || (popd & goto :fail)
"%MSYS2_ROOT%\mingw64\bin\gcc.exe" -O2 -mwindows -march=x86-64 -o "%BUILD%\minesweeper-x64.exe" minesweeper.c network.c analyze.c diag.c leader.c "%BUILD%\resources-x64.o" -luser32 -lgdi32 -lcomctl32 -lws2_32 -lwinhttp -ladvapi32 || (popd & goto :fail)
popd
echo [OK] build\minesweeper-x64.exe

rem ---------------- x86 ----------------
set PATH=%MSYS2_ROOT%\mingw32\bin;%PATH%
pushd "%SRC%"
"%MSYS2_ROOT%\mingw32\bin\windres.exe" resources.rc -o "%BUILD%\resources-x86.o" || (popd & goto :fail)
"%MSYS2_ROOT%\mingw32\bin\gcc.exe" -O2 -mwindows -march=i686 -o "%BUILD%\minesweeper-x86.exe" minesweeper.c network.c analyze.c diag.c leader.c "%BUILD%\resources-x86.o" -luser32 -lgdi32 -lcomctl32 -lws2_32 -lwinhttp -ladvapi32 || (popd & goto :fail)
popd
echo [OK] build\minesweeper-x86.exe

echo.
echo Build complete.
exit /b 0

:fail
echo BUILD FAILED
exit /b 1

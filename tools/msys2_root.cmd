@echo off
rem msys2_root.cmd - single source of truth for MSYS2_ROOT in local dev scripts.
rem Usage:  call "%~dp0msys2_root.cmd"
rem
rem Precedence:
rem   1. MSYS2_ROOT already set in the environment (CI sets it via
rem      msys2/setup-msys2, e.g. build-all.yml; anything explicit wins).
rem   2. The documented local default (scoop install of msys2) if present.
rem   3. A common MSYS2 install root (C:\msys64) if present.
rem
rem If none is found, MSYS2_ROOT is left undefined and the caller should
rem report a clear error.  Never edits the machine or user environment.
if defined MSYS2_ROOT goto :eof

if exist "C:\Users\Enis Polat\scoop\apps\msys2\current\mingw64\bin\gcc.exe" (
  set "MSYS2_ROOT=C:\Users\Enis Polat\scoop\apps\msys2\current"
  goto :eof
)
if exist "C:\msys64\mingw64\bin\gcc.exe" (
  set "MSYS2_ROOT=C:\msys64"
  goto :eof
)
goto :eof

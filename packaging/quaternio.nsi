Unicode True
!include "MUI2.nsh"

Name "Quaternio"
OutFile "..\dist\Quaternio-Setup.exe"
InstallDir "$LOCALAPPDATA\Quaternio"
RequestExecutionLevel user
InstallDirRegKey HKCU "Software\Quaternio" "InstallDir"

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Quaternio"
  SetOutPath "$INSTDIR"
  File "stage\quaternio.exe"
  File "stage\ffmpeg.exe"
  File "stage\ffprobe.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\Quaternio"
  CreateShortcut "$SMPROGRAMS\Quaternio\Quaternio.lnk" "$INSTDIR\quaternio.exe"
  CreateShortcut "$SMPROGRAMS\Quaternio\Uninstall Quaternio.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "Software\Quaternio" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quaternio" "DisplayName" "Quaternio"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quaternio" "DisplayVersion" "0.1.0-beta.1"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quaternio" "Publisher" "Quaternio"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quaternio" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quaternio" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quaternio" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quaternio" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\quaternio.exe"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\ffprobe.exe"
  Delete "$INSTDIR\uninstall.exe"
  Delete "$SMPROGRAMS\Quaternio\Quaternio.lnk"
  Delete "$SMPROGRAMS\Quaternio\Uninstall Quaternio.lnk"
  RMDir "$SMPROGRAMS\Quaternio"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Quaternio"
  DeleteRegKey HKCU "Software\Quaternio"
SectionEnd

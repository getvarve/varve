; Hooks for the NSIS installer (tauri.conf.json bundle > windows > nsis > installerHooks).
;
; Varve was called "Varve Community" up to version 0.1.7. The installer derives its folder
; ($LOCALAPPDATA\<product name>), its uninstall key and its shortcut names from the product name,
; so without these hooks Varve would install next to the old app instead of replacing it. The
; hooks find the old install through its uninstall key, refuse while it runs, and remove it with
; its own uninstaller once Varve is installed.
;
; Your data stays. It lives in %APPDATA%\Varve (data_dir in app/src-tauri/src/lib.rs), and the
; secrets in the Windows Credential Manager (service "varve"); neither depends on the product
; name. The old uninstaller deletes app data only when its app data box is ticked, which a silent
; run never does, and even then only the %APPDATA% and %LOCALAPPDATA% folders named after the
; bundle id (com.getvarve.desktop).

!define VARVE_LEGACY_NAME "Varve Community"
!define VARVE_LEGACY_UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${VARVE_LEGACY_NAME}"
!define VARVE_LEGACY_TASKBAR_PIN "$APPDATA\Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar\${VARVE_LEGACY_NAME}.lnk"

Var VarveLegacyDir

; Points an existing shortcut at a new exe and starts it in that exe's folder. The template's
; SetShortcutTarget sets only the target, which leaves "Start in" at the old, removed folder.
!macro VARVE_RETARGET_SHORTCUT shortcut target dir
  !insertmacro ComHlpr_CreateInProcInstance ${CLSID_ShellLink} ${IID_IShellLink} r0 ""
  ${If} $0 P<> 0
    ${IUnknown::QueryInterface} $0 '("${IID_IPersistFile}",.r1)'
    ${If} $1 P<> 0
      ${IPersistFile::Load} $1 '("${shortcut}", ${STGM_READWRITE})'
      ${IShellLink::SetPath} $0 '(w "${target}")'
      ${IShellLink::SetWorkingDirectory} $0 '(w "${dir}")'
      ${IPersistFile::Save} $1 '("${shortcut}",1)'
      ${IUnknown::Release} $1 ""
    ${EndIf}
    ${IUnknown::Release} $0 ""
  ${EndIf}
!macroend

; Sets $VarveLegacyDir to the folder of an old Varve Community install, or "" when there is none.
; The installers were per user (HKCU), and they write InstallLocation in quotes.
!macro VARVE_FIND_LEGACY
  ReadRegStr $VarveLegacyDir HKCU "${VARVE_LEGACY_UNINSTKEY}" "InstallLocation"
  StrCpy $R0 $VarveLegacyDir 1
  ${If} $R0 == '"'
    StrCpy $VarveLegacyDir $VarveLegacyDir -1 1
  ${EndIf}
!macroend

; Before any file is copied: the old app must not run, or its uninstaller would close it without
; asking. A running exe cannot be opened for writing. The in-app updater starts this installer
; and then quits, so give it a few seconds first.
!macro NSIS_HOOK_PREINSTALL
  !insertmacro VARVE_FIND_LEGACY
  ${If} $VarveLegacyDir != ""
  ${AndIf} ${FileExists} "$VarveLegacyDir\${MAINBINARYNAME}.exe"
    StrCpy $R1 0
    ${Do}
      ClearErrors
      FileOpen $R2 "$VarveLegacyDir\${MAINBINARYNAME}.exe" a
      ${IfNot} ${Errors}
        FileClose $R2
        ${Break}
      ${EndIf}
      IntOp $R1 $R1 + 1
      ${If} $R1 < 20
        Sleep 500
      ${ElseIf} ${Silent}
      ${OrIf} $PassiveMode = 1
        Abort "${VARVE_LEGACY_NAME} is running. Quit it and run the installer again."
      ${ElseIf} ${Cmd} `MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${VARVE_LEGACY_NAME} is running. Quit it, then click Retry." IDCANCEL`
        Abort "${VARVE_LEGACY_NAME} is running. Quit it and run the installer again."
      ${EndIf}
    ${Loop}
  ${EndIf}
!macroend

; After Varve is installed: remove the old install.
;
; The old uninstaller runs in update mode (/UPDATE). Outside update mode it would unpin its
; shortcuts and clear the jump list of the app id com.getvarve.desktop, which Varve shares, so
; Varve would lose them. In update mode it leaves the shortcuts alone; they are replaced here.
!macro NSIS_HOOK_POSTINSTALL
  !insertmacro VARVE_FIND_LEGACY
  ${If} $VarveLegacyDir != ""
    StrCpy $R5 0
    ${If} $VarveLegacyDir == $INSTDIR
      ; Installed into the old folder: its files are Varve's now, so its uninstaller must not
      ; run. Only the old entries go.
      StrCpy $R5 1
    ${ElseIf} ${FileExists} "$VarveLegacyDir\uninstall.exe"
      DetailPrint "Removing ${VARVE_LEGACY_NAME} from $VarveLegacyDir"
      ; /S: silent, so the app data box stays unticked. _?= runs the uninstaller in place, so
      ; ExecWait waits for it; it then cannot delete itself and its folder, so that is done here.
      ClearErrors
      ExecWait '"$VarveLegacyDir\uninstall.exe" /S /UPDATE _?=$VarveLegacyDir' $R6
      ${If} ${Errors}
      ${OrIf} $R6 <> 0
        DetailPrint "Could not remove ${VARVE_LEGACY_NAME} (exit code $R6). Remove it in Settings > Apps > Installed apps."
      ${Else}
        Delete "$VarveLegacyDir\uninstall.exe"
        RMDir "$VarveLegacyDir"
        StrCpy $R5 1
      ${EndIf}
    ${Else}
      ; The folder is gone already: only the stale entry in Installed apps is left.
      StrCpy $R5 1
    ${EndIf}

    ${If} $R5 = 1
      ; The uninstaller keeps the key that holds the old install folder and language; drop it.
      DeleteRegKey HKCU "${VARVE_LEGACY_UNINSTKEY}"
      DeleteRegKey HKCU "${MANUKEY}\${VARVE_LEGACY_NAME}"
      DeleteRegKey /ifempty HKCU "${MANUKEY}"
      ; Replace the old shortcuts with ones under the new name. An update (/UPDATE) creates no
      ; shortcuts of its own, a fresh install has created them already.
      ${If} ${FileExists} "$SMPROGRAMS\${VARVE_LEGACY_NAME}.lnk"
        Delete "$SMPROGRAMS\${VARVE_LEGACY_NAME}.lnk"
        ${IfNot} ${FileExists} "$SMPROGRAMS\${PRODUCTNAME}.lnk"
          CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
          !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
        ${EndIf}
      ${EndIf}
      ${If} ${FileExists} "$DESKTOP\${VARVE_LEGACY_NAME}.lnk"
        Delete "$DESKTOP\${VARVE_LEGACY_NAME}.lnk"
        ${IfNot} ${FileExists} "$DESKTOP\${PRODUCTNAME}.lnk"
          CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
          !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
        ${EndIf}
      ${EndIf}
      ; A taskbar pin is a copy of the shortcut that still starts the old exe. Point it at
      ; Varve so the pin keeps working.
      !insertmacro IsShortcutTarget "${VARVE_LEGACY_TASKBAR_PIN}" "$VarveLegacyDir\${MAINBINARYNAME}.exe"
      Pop $0
      ${If} $0 = 1
        !insertmacro VARVE_RETARGET_SHORTCUT "${VARVE_LEGACY_TASKBAR_PIN}" "$INSTDIR\${MAINBINARYNAME}.exe" "$INSTDIR"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend

; SYLVER's brain is bigger than an installer can hold (2 GB limit), so it ships right
; next to the installer as "SYLVER-Brain.gguf" and is copied in during setup. SYLVER is
; then fully smart from the first second, with nothing to download.

!macro NSIS_HOOK_POSTINSTALL
  IfFileExists "$EXEDIR\SYLVER-Brain.gguf" 0 +4
    CreateDirectory "$APPDATA\com.ultron.notch\models"
    DetailPrint "Installing SYLVER's brain..."
    CopyFiles /SILENT "$EXEDIR\SYLVER-Brain.gguf" "$APPDATA\com.ultron.notch\models\SYLVER-Brain.gguf"
!macroend

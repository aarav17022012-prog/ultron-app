; ULTRON's brain is bigger than an installer can hold (2 GB limit), so it ships right
; next to the installer as "ULTRON-Brain.gguf" and is copied in during setup. ULTRON is
; then fully smart from the first second, with nothing to download.

!macro NSIS_HOOK_POSTINSTALL
  IfFileExists "$EXEDIR\ULTRON-Brain.gguf" 0 +4
    CreateDirectory "$APPDATA\com.ultron.notch\models"
    DetailPrint "Installing ULTRON's brain..."
    CopyFiles /SILENT "$EXEDIR\ULTRON-Brain.gguf" "$APPDATA\com.ultron.notch\models\ULTRON-Brain.gguf"
!macroend

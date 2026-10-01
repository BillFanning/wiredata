# Ask a console process to stop as an operator's Ctrl-C would (Windows).
#
#   pwsh -File send-ctrl-c.ps1 -Id <process id>
#
# A script cannot press Ctrl-C in another process's console, and killing the
# process skips its graceful stop. This attaches to the target's console and
# raises Ctrl-C there, so the process runs its own stop and exits with its
# own code. Start the target in a console of its own (Start-Process without
# -NoNewWindow), or the Ctrl-C would reach this script's console too.
# On Linux, `kill -TERM <pid>` does the same job.

param([Parameter(Mandatory = $true)][int]$Id)

Add-Type -Namespace Soak -Name Console -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true)] public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError = true)] public static extern bool AttachConsole(uint id);
[DllImport("kernel32.dll", SetLastError = true)] public static extern bool SetConsoleCtrlHandler(System.IntPtr handler, bool add);
[DllImport("kernel32.dll", SetLastError = true)] public static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint group);
'@

# Leave this script's console, join the target's, and ignore Ctrl-C here so
# only the target acts on it.
[Soak.Console]::FreeConsole() | Out-Null
if (-not [Soak.Console]::AttachConsole([uint32]$Id)) {
    Write-Error "cannot attach to the console of process $Id"
    exit 1
}
[Soak.Console]::SetConsoleCtrlHandler([System.IntPtr]::Zero, $true) | Out-Null
# CTRL_C_EVENT is 0; group 0 is every process attached to this console.
if (-not [Soak.Console]::GenerateConsoleCtrlEvent(0, 0)) {
    exit 1
}
exit 0

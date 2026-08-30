param(
    [Parameter(Mandatory = $true)]
    [string]$ExePath
)

$ErrorActionPreference = "Stop"
$resolvedExe = (Resolve-Path -LiteralPath $ExePath).Path

if (-not ("PeResourceInspector" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public static class PeResourceInspector
{
    private const uint LoadLibraryAsDataFile = 0x00000002;
    private const uint LoadLibraryAsImageResource = 0x00000020;
    private static readonly IntPtr GroupIconResource = new IntPtr(14);

    private delegate bool EnumResourceNameCallback(
        IntPtr module,
        IntPtr resourceType,
        IntPtr resourceName,
        IntPtr parameter);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr LoadLibraryEx(
        string fileName,
        IntPtr file,
        uint flags);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool FreeLibrary(IntPtr module);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool EnumResourceNames(
        IntPtr module,
        IntPtr resourceType,
        EnumResourceNameCallback callback,
        IntPtr parameter);

    public static bool HasGroupIcon(string path)
    {
        IntPtr module = LoadLibraryEx(
            path,
            IntPtr.Zero,
            LoadLibraryAsDataFile | LoadLibraryAsImageResource);
        if (module == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                "Could not load executable resources. Win32 error: " +
                Marshal.GetLastWin32Error());
        }

        try
        {
            bool found = false;
            EnumResourceNameCallback callback = (handle, type, name, value) =>
            {
                found = true;
                return false;
            };
            EnumResourceNames(module, GroupIconResource, callback, IntPtr.Zero);
            GC.KeepAlive(callback);
            return found;
        }
        finally
        {
            FreeLibrary(module);
        }
    }
}
"@
}

if (-not [PeResourceInspector]::HasGroupIcon($resolvedExe)) {
    throw "Windows GROUP_ICON resource is missing from $resolvedExe"
}

Write-Host "Windows icon resource verified: $resolvedExe"

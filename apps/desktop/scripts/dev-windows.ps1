# @author jvz
# PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
# Only this entry owns Windows dev processes. No fallback to an unowned launch.
[CmdletBinding()]
param(
    [switch]$Preview,
    [switch]$WatchOwner,
    [switch]$PreviewParentInput,
    [uint32]$OwnerId,
    [string]$OwnerBirth,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$TauriArguments
)
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'dev:windows requires Windows.' }

Add-Type -TypeDefinition @'
// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
using System;
using System.Collections;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Security.Cryptography;
using System.Text;
using System.Threading;

public static class PureFlowWindowsDev {
    const uint WAIT_OBJECT_0 = 0, INFINITE = 0xffffffff;
    const uint SYNCHRONIZE = 0x100000, QUERY_LIMITED_INFORMATION = 0x1000;
    const uint CREATE_SUSPENDED = 4, CREATE_UNICODE_ENVIRONMENT = 0x400;
    const uint CREATE_NEW_PROCESS_GROUP = 0x200;
    static IntPtr job;
    static IntPtr stop;
    static int shuttingDown;
    static readonly object stopLock = new object();
    static ConsoleHandler consoleHandler;
    delegate bool ConsoleHandler(uint signal);

    [StructLayout(LayoutKind.Sequential)] struct IO_COUNTERS {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
        public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }
    [StructLayout(LayoutKind.Sequential)] struct BASIC_LIMITS {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass, SchedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)] struct EXTENDED_LIMITS {
        public BASIC_LIMITS BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct STARTUPINFO {
        public uint cb;
        public string reserved, desktop, title;
        public uint x, y, xSize, ySize, xCountChars, yCountChars, fillAttribute, flags;
        public ushort showWindow, reserved2Size;
        public IntPtr reserved2, stdInput, stdOutput, stdError;
    }
    [StructLayout(LayoutKind.Sequential)] struct STARTUPINFOEX {
        public STARTUPINFO startup;
        public IntPtr attributes;
    }
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION {
        public IntPtr process, thread;
        public uint processId, threadId;
    }
    [StructLayout(LayoutKind.Sequential)] struct FILETIME {
        public uint low, high;
        public ulong Value { get { return ((ulong)high << 32) | low; } }
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr job, int kind, ref EXTENDED_LIMITS limits, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetHandleInformation(IntPtr handle, uint mask, uint flags);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFile(string path, uint access, uint sharing, IntPtr security, uint creation, uint flags, IntPtr template);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern uint GetFinalPathNameByHandle(IntPtr file, StringBuilder path, uint size, uint flags);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcess(string application, StringBuilder command, IntPtr processAttributes,
        IntPtr threadAttributes, bool inheritHandles, uint flags, IntPtr environment, string directory,
        ref STARTUPINFO startup, out PROCESS_INFORMATION info);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateProcessW")]
    static extern bool CreateProcessWithHandles(string application, StringBuilder command, IntPtr processAttributes,
        IntPtr threadAttributes, bool inheritHandles, uint flags, IntPtr environment, string directory,
        ref STARTUPINFOEX startup, out PROCESS_INFORMATION info);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, uint flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attribute, IntPtr value,
        IntPtr size, IntPtr previous, IntPtr returnedSize);
    [DllImport("kernel32.dll")] static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int kind);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DuplicateHandle(IntPtr sourceProcess, IntPtr source, IntPtr targetProcess,
        out IntPtr target, uint access, bool inherit, uint options);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr handle, uint timeout);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForMultipleObjects(uint count, IntPtr[] handles, bool all, uint timeout);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr process, out uint code);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetProcessTimes(IntPtr process, out FILETIME birth, out FILETIME exit, out FILETIME kernel, out FILETIME user);
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint id);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateEvent(IntPtr attributes, bool manual, bool initial, string name);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetEvent(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetConsoleCtrlHandler(ConsoleHandler handler, bool add);

    static void Check(bool ok, string operation) {
        if (!ok) throw new Win32Exception(Marshal.GetLastWin32Error(), operation);
    }
    static ulong Birth(IntPtr process) {
        FILETIME birth, exit, kernel, user;
        Check(GetProcessTimes(process, out birth, out exit, out kernel, out user), "GetProcessTimes");
        return birth.Value;
    }
    static void CloseJob() {
        IntPtr handle = Interlocked.Exchange(ref job, IntPtr.Zero);
        if (handle != IntPtr.Zero) CloseHandle(handle);
    }
    static void SignalStop() {
        lock (stopLock) { if (stop != IntPtr.Zero) SetEvent(stop); }
    }
    static void CloseStop() {
        lock (stopLock) {
            if (stop != IntPtr.Zero) { CloseHandle(stop); stop = IntPtr.Zero; }
        }
    }
    static bool OnConsole(uint signal) {
        // Also runs on console-window close/logoff/shutdown, not just Ctrl+C.
        Interlocked.Exchange(ref shuttingDown, 1);
        CloseJob();
        SignalStop();
        return true;
    }
    static string Quote(string value) {
        // CommandLineToArgvW/CRT quoting, including trailing backslashes.
        var result = new StringBuilder("\"");
        int slashes = 0;
        foreach (char c in value) {
            if (c == '\\') { slashes++; continue; }
            if (c == '"') { result.Append('\\', slashes * 2 + 1); result.Append(c); }
            else { result.Append('\\', slashes); result.Append(c); }
            slashes = 0;
        }
        result.Append('\\', slashes * 2);
        return result.Append('"').ToString();
    }
    static void CreatePreviewProcess(string node, StringBuilder command, IntPtr environment,
        string directory, out PROCESS_INFORMATION child) {
        // Never inherit the bridge's lifetime stdin or arbitrary owner handles.
        // Explicitly pass only NUL input and output duplicates; the Job stays private.
        IntPtr nul = IntPtr.Zero, list = IntPtr.Zero, values = IntPtr.Zero;
        var handles = new IntPtr[3];
        bool initialized = false;
        try {
            nul = CreateFile("NUL", 0x80000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
            Check(nul != new IntPtr(-1), "Open preview NUL input");
            IntPtr current = GetCurrentProcess();
            IntPtr[] sources = { nul, GetStdHandle(-11), GetStdHandle(-12) };
            for (int i = 0; i < handles.Length; i++)
                Check(DuplicateHandle(current, sources[i], current, out handles[i], 0, true, 2), "Duplicate preview standard handle");
            IntPtr size = IntPtr.Zero;
            InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref size);
            list = Marshal.AllocHGlobal(size);
            Check(InitializeProcThreadAttributeList(list, 1, 0, ref size), "Initialize preview handle list");
            initialized = true;
            values = Marshal.AllocHGlobal(IntPtr.Size * handles.Length);
            Marshal.Copy(handles, 0, values, handles.Length);
            Check(UpdateProcThreadAttribute(list, 0, new IntPtr(0x20002), values,
                new IntPtr(IntPtr.Size * handles.Length), IntPtr.Zero, IntPtr.Zero), "Restrict preview inherited handles");
            var startup = new STARTUPINFOEX();
            startup.startup.cb = (uint)Marshal.SizeOf(startup);
            startup.startup.flags = 0x100; // STARTF_USESTDHANDLES
            startup.startup.stdInput = handles[0];
            startup.startup.stdOutput = handles[1];
            startup.startup.stdError = handles[2];
            startup.attributes = list;
            Check(CreateProcessWithHandles(node, command, IntPtr.Zero, IntPtr.Zero, true,
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP | 0x80000,
                environment, directory, ref startup, out child), "Create suspended preview root");
        } finally {
            if (initialized) DeleteProcThreadAttributeList(list);
            if (list != IntPtr.Zero) Marshal.FreeHGlobal(list);
            if (values != IntPtr.Zero) Marshal.FreeHGlobal(values);
            foreach (IntPtr handle in handles) if (handle != IntPtr.Zero) CloseHandle(handle);
            if (nul != IntPtr.Zero && nul != new IntPtr(-1)) CloseHandle(nul);
        }
    }
    static NamedPipeServerStream NewPipe(string name) {
        var security = new PipeSecurity();
        security.SetAccessRuleProtection(true, false);
        security.AddAccessRule(new PipeAccessRule(WindowsIdentity.GetCurrent().User,
            PipeAccessRights.FullControl, AccessControlType.Allow));
        security.AddAccessRule(new PipeAccessRule(new SecurityIdentifier(WellKnownSidType.LocalSystemSid, null),
            PipeAccessRights.FullControl, AccessControlType.Allow));
        return new NamedPipeServerStream(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte,
            PipeOptions.Asynchronous, 4096, 4096, security);
    }
    static Mutex SessionLease(string directory) {
        // Object lifetime, not a stale file/PID. GUI dev and browser preview share
        // one per-user canonical-worktree lease, including across logon sessions.
        using (var current = WindowsIdentity.GetCurrent()) {
        string canonical;
        IntPtr root = CreateFile(Path.GetFullPath(directory), 0x80, 7, IntPtr.Zero, 3, 0x02000000, IntPtr.Zero);
        Check(root != new IntPtr(-1), "Open worktree directory");
        try {
            var path = new StringBuilder(512);
            uint length = GetFinalPathNameByHandle(root, path, (uint)path.Capacity, 0);
            if (length >= path.Capacity) {
                path = new StringBuilder(checked((int)length + 1));
                length = GetFinalPathNameByHandle(root, path, (uint)path.Capacity, 0);
            }
            Check(length > 0 && length < path.Capacity, "Resolve canonical worktree directory");
            canonical = path.ToString().TrimEnd(Path.DirectorySeparatorChar).ToUpperInvariant();
        } finally { CloseHandle(root); }
        string user = current.User.Value;
        string hash;
        using (var sha = SHA256.Create())
            hash = BitConverter.ToString(sha.ComputeHash(Encoding.UTF8.GetBytes(user + "\n" + canonical))).Replace("-", "");
        var security = new MutexSecurity();
        security.SetAccessRuleProtection(true, false);
        security.AddAccessRule(new MutexAccessRule(current.User,
            MutexRights.FullControl, AccessControlType.Allow));
        security.AddAccessRule(new MutexAccessRule(new SecurityIdentifier(WellKnownSidType.LocalSystemSid, null),
            MutexRights.FullControl, AccessControlType.Allow));
        bool created;
        var lease = new Mutex(false, "Global\\PureFlow-dev-" + hash, out created, security);
        if (!created) {
            lease.Dispose();
            throw new InvalidOperationException("This user already has a PureFlow dev/preview session for this worktree. Close its owner first.");
        }
        return lease;
        }
    }
    public static int Launch(string node, string cli, string directory, string[] arguments, bool preview, bool parentInput) {
        using (var identity = WindowsIdentity.GetCurrent()) {
            if (new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator))
                throw new InvalidOperationException("Start dev:windows without elevation; only the collector may request UAC.");
        }
        Mutex lease = SessionLease(directory);
        PROCESS_INFORMATION child = new PROCESS_INFORMATION();
        IntPtr environment = IntPtr.Zero;
        NamedPipeServerStream pipe = null;
        Thread publisher = null;
        var connected = new ManualResetEvent(false);
        bool assigned = false, resumed = false;
        try {
        try {
            job = CreateJobObject(IntPtr.Zero, null);
            Check(job != IntPtr.Zero, "CreateJobObject");
            Check(SetHandleInformation(job, 1, 0), "Make job handle non-inheritable");
            var limits = new EXTENDED_LIMITS();
            limits.BasicLimitInformation.LimitFlags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            Check(SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf(limits)), "Set kill-on-close job limit");
            stop = CreateEvent(IntPtr.Zero, true, false, null);
            Check(stop != IntPtr.Zero, "Create shutdown event");
            consoleHandler = OnConsole;
            Check(SetConsoleCtrlHandler(consoleHandler, true), "Install console shutdown handler");

            string pipeName = "PureFlow-dev-" + Guid.NewGuid().ToString("N");
            pipe = NewPipe(pipeName);
            var variables = new SortedDictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            foreach (DictionaryEntry entry in Environment.GetEnvironmentVariables())
                variables[(string)entry.Key] = (string)entry.Value;
            variables["PUREFLOW_WINDOWS_DEV_PIPE"] = "\\\\.\\pipe\\" + pipeName;
            var block = new StringBuilder();
            foreach (var entry in variables) block.Append(entry.Key).Append('=').Append(entry.Value).Append('\0');
            block.Append('\0');
            environment = Marshal.StringToHGlobalUni(block.ToString());
            var command = new StringBuilder(Quote(node) + " " + Quote(cli) + (preview ? "" : " dev"));
            foreach (string argument in arguments) command.Append(' ').Append(Quote(argument));
            var startup = new STARTUPINFO();
            startup.cb = (uint)Marshal.SizeOf(startup);
            if (preview) CreatePreviewProcess(node, command, environment, directory, out child);
            else Check(CreateProcess(node, command, IntPtr.Zero, IntPtr.Zero, false,
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP,
                environment, directory, ref startup, out child), "Create suspended Tauri CLI");
            Check(AssignProcessToJobObject(job, child.process), "Assign Tauri CLI to job (no unowned fallback)");
            assigned = true;
            string identity = child.processId + " " + Birth(child.process) + "\n";
            NamedPipeServerStream identityPipe = pipe;
            publisher = new Thread(() => {
                try {
                    // Tauri can restart beforeDevCommand: each wrapper binds the same CLI handle.
                    while (true) {
                        identityPipe.WaitForConnection();
                        byte[] bytes = Encoding.UTF8.GetBytes(identity);
                        identityPipe.Write(bytes, 0, bytes.Length);
                        identityPipe.Flush();
                        // Do not disconnect (and discard unread bytes) before the client reads identity.
                        if (identityPipe.ReadByte() != 10) throw new IOException("Owner identity was not acknowledged.");
                        connected.Set();
                        identityPipe.Disconnect();
                    }
                } catch (ObjectDisposedException) { }
                  catch (Exception error) {
                    if (Interlocked.CompareExchange(ref shuttingDown, 0, 0) != 0) return;
                    Console.Error.WriteLine("PureFlow Windows owner handshake failed: " + error.Message);
                    CloseJob();
                    SignalStop();
                }
            });
            publisher.IsBackground = true;
            publisher.Start();
            Check(WaitForSingleObject(stop, 0) == 0x102, "Launcher already stopping");
            Check(ResumeThread(child.thread) != 0xffffffff, "Resume owned Tauri CLI");
            resumed = true;
            if (parentInput) {
                // This private lifetime pipe belongs only to the bridge and owner.
                // Initialize it after process creation, never pass it to Node.
                var parentStream = Console.OpenStandardInput();
                var input = new Thread(() => {
                    try { parentStream.ReadByte(); }
                    catch (IOException) { } // A broken lifetime pipe is parent loss too.
                    finally { CloseJob(); SignalStop(); }
                });
                input.IsBackground = true;
                input.Start();
            }
            uint result;
            if (preview) {
                result = WaitForMultipleObjects(3, new IntPtr[] {
                    child.process, stop, connected.SafeWaitHandle.DangerousGetHandle()
                }, false, 30000);
                if (result == 0x102) throw new TimeoutException("Preview did not bind its lifecycle owner within 30 seconds.");
                Check(result <= WAIT_OBJECT_0 + 2, "Wait for preview startup");
                if (result == WAIT_OBJECT_0 + 1) return 130;
            }
            result = WaitForMultipleObjects(2, new IntPtr[] { child.process, stop }, false, INFINITE);
            if (result == WAIT_OBJECT_0 + 1) return 130;
            Check(result == WAIT_OBJECT_0, "Wait for Tauri CLI");
            uint code;
            Check(GetExitCodeProcess(child.process, out code), "Get Tauri CLI exit code");
            return unchecked((int)code);
        } finally {
            try {
                // Even assignment/resume failures never leave a running unowned child.
                if (child.process != IntPtr.Zero && (!assigned || !resumed)) {
                    if (WaitForSingleObject(child.process, 0) != WAIT_OBJECT_0)
                        Check(TerminateProcess(child.process, 1), "Terminate suspended child after ownership failure");
                    Check(WaitForSingleObject(child.process, INFINITE) == WAIT_OBJECT_0, "Confirm failed child exited");
                }
            } finally {
                Interlocked.Exchange(ref shuttingDown, 1);
                CloseJob();
                if (child.process != IntPtr.Zero && assigned && resumed &&
                    WaitForSingleObject(child.process, 6000) != WAIT_OBJECT_0)
                    Console.Error.WriteLine("PureFlow owned root did not confirm exit within 6 seconds; descendant cleanup is unconfirmed.");
                try {
                    if (pipe != null) pipe.Dispose();
                    // Never hang teardown on a client that disappeared mid-handshake.
                    if (publisher != null && !publisher.Join(1000))
                        Console.Error.WriteLine("PureFlow owner publisher did not confirm exit before its process teardown.");
                } finally {
                    if (consoleHandler != null) SetConsoleCtrlHandler(consoleHandler, false);
                    CloseStop();
                    if (child.thread != IntPtr.Zero) CloseHandle(child.thread);
                    if (child.process != IntPtr.Zero) CloseHandle(child.process);
                    if (environment != IntPtr.Zero) Marshal.FreeHGlobal(environment);
                }
            }
        }
        } finally {
            // A stuck background publisher must never access a disposed event.
            if (publisher == null || !publisher.IsAlive) connected.Dispose();
            lease.Dispose();
        }
    }
    public static int Watch(uint id, ulong expectedBirth) {
        IntPtr owner = OpenProcess(SYNCHRONIZE | QUERY_LIMITED_INFORMATION, false, id);
        Check(owner != IntPtr.Zero, "Open actual Tauri owner process");
        using (var inputClosed = new ManualResetEvent(false)) {
            try {
                if (Birth(owner) != expectedBirth) throw new InvalidOperationException("Tauri owner identity changed (PID reuse).");
                if (WaitForSingleObject(owner, 0) != 0x102) throw new InvalidOperationException("Tauri owner already exited.");
                Console.WriteLine("OWNER_READY");
                Console.Out.Flush();
                var input = new Thread(() => {
                    Console.In.ReadLine();
                    try { inputClosed.Set(); } catch (ObjectDisposedException) { }
                });
                input.IsBackground = true;
                input.Start();
                uint result = WaitForMultipleObjects(2, new IntPtr[] { owner, inputClosed.SafeWaitHandle.DangerousGetHandle() }, false, INFINITE);
                Check(result == WAIT_OBJECT_0 || result == WAIT_OBJECT_0 + 1, "Wait for owner process handle");
                return result == WAIT_OBJECT_0 ? 10 : 0;
            } finally { CloseHandle(owner); }
        }
    }
}
'@

try {
    if ($WatchOwner) {
        exit [PureFlowWindowsDev]::Watch($OwnerId, [uint64]::Parse($OwnerBirth, [Globalization.CultureInfo]::InvariantCulture))
    }
    $desktop = Split-Path -Parent $PSScriptRoot
    $node = (Get-Command node.exe -CommandType Application -ErrorAction Stop).Source
    $cli = if ($Preview) { Join-Path $desktop 'scripts\dev-preview-windows.mjs' } else { Join-Path $desktop 'node_modules\@tauri-apps\cli\tauri.js' }
    if (-not (Test-Path -LiteralPath $cli -PathType Leaf)) { throw 'Run npm ci in apps/desktop first.' }
    exit [PureFlowWindowsDev]::Launch($node, $cli, $desktop, [string[]]@($TauriArguments), [bool]$Preview, [bool]$PreviewParentInput)
} catch {
    [Console]::Error.WriteLine('PureFlow Windows dev lifecycle failed: ' + $_.Exception.Message)
    exit 1
}

$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class EngineBench {
 public delegate bool EnumProc(IntPtr h,IntPtr d);
 [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc callback,IntPtr d);
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr h,StringBuilder name,int length);
 [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr h,int id);
 [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll")] public static extern bool SendMessageTimeout(IntPtr h,uint m,IntPtr w,IntPtr l,uint flags,uint timeout,out UIntPtr result);
 [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h,IntPtr after,int x,int y,int w,int height,uint flags);
 [DllImport("user32.dll")] public static extern bool RedrawWindow(IntPtr h,IntPtr rect,IntPtr region,uint flags);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h,out Rect r);
 [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h,IntPtr dc,uint flags);
 [StructLayout(LayoutKind.Sequential)] public struct Rect {public int Left,Top,Right,Bottom;}
 public static IntPtr Window(int process,string cls){IntPtr found=IntPtr.Zero;EnumWindows((h,d)=>{uint id;GetWindowThreadProcessId(h,out id);if(id==process){var name=new StringBuilder(128);GetClassName(h,name,name.Capacity);if(name.ToString()==cls){found=h;return false;}}return true;},IntPtr.Zero);return found;}
 public static long Count(IntPtr main,bool shell){var hwnd=shell?main:GetDlgItem(main,108);if(hwnd==IntPtr.Zero)return -1;UIntPtr value;if(!SendMessageTimeout(hwnd,shell?0x81F5u:4100u,IntPtr.Zero,IntPtr.Zero,2,500,out value))return -1;return unchecked((long)value.ToUInt64());}
}
'@
function Get-ExecutableHash([string]$path){$stream=[IO.File]::OpenRead($path);$sha=[Security.Cryptography.SHA256]::Create();try{[BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-','')}finally{$sha.Dispose();$stream.Dispose()}}
function Save-Window($window,[string]$file){$r=New-Object EngineBench+Rect;[void][EngineBench]::GetWindowRect($window,[ref]$r);$image=New-Object Drawing.Bitmap ($r.Right-$r.Left),($r.Bottom-$r.Top);$g=[Drawing.Graphics]::FromImage($image);$dc=$g.GetHdc();try{if(-not[EngineBench]::PrintWindow($window,$dc,2)){throw 'PrintWindow failed'}}finally{$g.ReleaseHdc($dc)};$image.Save($file,[Drawing.Imaging.ImageFormat]::Png);$g.Dispose();$image.Dispose()}
$fixture=Join-Path $PSScriptRoot ('target\engine-bench-'+[Guid]::NewGuid().ToString('N'))
$report=Join-Path $PSScriptRoot 'target\engine-benchmark-current.json'
$custom=Join-Path $PSScriptRoot 'target\release\rfm.exe'
$shell=Join-Path $PSScriptRoot 'target\release\examples\shell_probe.exe'
# The filesystem cache is intentionally warm for both engines. These are not cold-boot figures.
$counts=@(100,10000,50000)
$results=New-Object Collections.Generic.List[object]
$previousLocalAppData=$env:LOCALAPPDATA
try{
 # Do not replace the user's saved folders/window placement during a benchmark.
 $env:LOCALAPPDATA=Join-Path $fixture 'profile'
 [void][IO.Directory]::CreateDirectory($env:LOCALAPPDATA)
foreach($count in $counts){$dir=Join-Path $fixture ([string]$count);[void][IO.Directory]::CreateDirectory($dir);for($i=0;$i -lt $count;$i++){[IO.File]::WriteAllText((Join-Path $dir ('item-{0:D6}.txt' -f $i)),'benchmark')}}
 foreach($count in $counts){
  # One warmup followed by five measured rounds. Reverse engine order on alternate rounds.
  for($round=0;$round -le 5;$round++){
   $order=if($round%2 -eq 0){@('custom','shell')}else{@('shell','custom')}
   foreach($engine in $order){
    $native=$engine -eq 'shell';$exe=if($native){$shell}else{$custom};$class=if($native){'rfm.shell.probe'}else{'rfm.main'}
    $folder=Join-Path $fixture ([string]$count)
    $clock=[Diagnostics.Stopwatch]::StartNew()
    $process=Start-Process -FilePath $exe -ArgumentList ('"'+$folder+'"') -PassThru
    $window=[IntPtr]::Zero
    try{
     while($window -eq [IntPtr]::Zero){if($process.HasExited){throw ($engine+' exited '+$process.ExitCode)};if($clock.ElapsedMilliseconds -gt 20000){throw 'window timeout'};$window=[EngineBench]::Window($process.Id,$class);if($window -eq [IntPtr]::Zero){Start-Sleep -Milliseconds 10}}
     [void][EngineBench]::SetWindowPos($window,[IntPtr]::Zero,24,24,1100,670,0x14)
     while([EngineBench]::Count($window,$native) -ne $count){if($process.HasExited){throw ($engine+' exited during listing')};if($clock.ElapsedMilliseconds -gt 60000){throw ($engine+' enumeration timeout '+$count)};Start-Sleep -Milliseconds 20}
     # Force the actual child controls to paint before counting the listing as ready.
     [void][EngineBench]::RedrawWindow($window,[IntPtr]::Zero,[IntPtr]::Zero,0x181)
     $ready=$clock.Elapsed.TotalMilliseconds
     Start-Sleep -Milliseconds 1500
     $process.Refresh()
     $row=[pscustomobject]@{engine=$engine;items=$count;round=$round;ready_ms=[Math]::Round($ready,2);cpu_ms=[Math]::Round($process.TotalProcessorTime.TotalMilliseconds,2);private_bytes=$process.PrivateMemorySize64;working_set=$process.WorkingSet64;peak_working_set=$process.PeakWorkingSet64;exe_bytes=(Get-Item -LiteralPath $exe).Length}
     if($round -gt 0){$results.Add($row)}
     Write-Output ($row|ConvertTo-Json -Compress)
     if($count -eq 100 -and $round -eq 1){Save-Window $window (Join-Path $PSScriptRoot ('target\engine-'+$engine+'.png'))}
    }finally{
     if(-not $process.HasExited){if($window -ne [IntPtr]::Zero){[void][EngineBench]::PostMessage($window,0x10,[IntPtr]::Zero,[IntPtr]::Zero)};if(-not $process.WaitForExit(30000)){$ownedPid=$process.Id;$process.Kill();[void]$process.WaitForExit(5000);throw ('Benchmark process required forced cleanup after close timeout: '+$ownedPid)}}
     if($process.ExitCode -ne 0){throw ('Benchmark process failed during shutdown: '+$process.Id+' exit='+$process.ExitCode)}
    }
   }
  }
 }
 $output=[pscustomobject]@{description='Release builds; same folders and 1100x670 window; details view; warm filesystem cache; warmup discarded; five measured rounds with alternating order; memory/CPU sampled 1.5 seconds after enumeration and forced paint. Current custom build includes shared Quick Access and filesystem notifications. Shell probe has native frames but no custom tabs/address header; functionality is not identical. Measurements are process-local, not whole-system costs or universal engine superiority.';timestamp=(Get-Date -Format o);fixture=$fixture;custom_sha256=(Get-ExecutableHash $custom);shell_sha256=(Get-ExecutableHash $shell);measurements=$results.ToArray()}
 [IO.File]::WriteAllText($report,($output|ConvertTo-Json -Depth 5),(New-Object Text.UTF8Encoding $false))
 Write-Output ('BENCHMARK_COMPLETE '+$report)
}finally{
 $env:LOCALAPPDATA=$previousLocalAppData
 if(Test-Path -LiteralPath $fixture){Remove-Item -LiteralPath $fixture -Recurse -Force}
}

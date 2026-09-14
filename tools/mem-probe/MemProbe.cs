using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.IO;
using System.Linq;

/// <summary>
/// MemProbe — 只读跨进程内存探测器（Windows）。
///
/// 用法（pwsh）：
///   $d = "tools\mem-probe"
///   Add-Type -TypeDefinition (Get-Content "$d\MemProbe.cs" -Raw) -Language CSharp
///   [MemProbe]::Regions($pid, "$d\out-regions.txt")      # 区域枚举 + 大小分桶
///   [MemProbe]::Composition($pid, "$d\out-comp.txt")     # 字节构成（ascii/utf8/zero/ctrl）
///   [MemProbe]::Markers($pid, "$d\out-markers.txt")      # 关键词出现次数
///   [MemProbe]::Dup($pid, "$d\out-dup.txt", 64)          # 可打印串去重（唯一 vs 重复）
///   [MemProbe]::Dump($pid, "$d\out-dump.txt", 8000, 12000, 4, 64)  # 指定尺寸区域抽样
///
/// 安全：全程只读（OpenProcess = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ），
///       不写目标进程、不改其状态。不含任何写内存调用。
/// </summary>
public class MemProbe {
  [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(int a, bool i, int pid);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool ReadProcessMemory(IntPtr h, IntPtr addr, byte[] buf, int size, out IntPtr read);
  [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr VirtualQueryEx(IntPtr h, IntPtr addr, out MEMORY_BASIC_INFORMATION mbi, IntPtr len);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr h);

  [StructLayout(LayoutKind.Sequential)]
  public struct MEMORY_BASIC_INFORMATION {
    public IntPtr BaseAddress; public IntPtr AllocationBase; public int AllocationProtect;
    public IntPtr RegionSize; public int State; public int Protect; public int Type;
  }

  const int PROCESS_QUERY_INFORMATION = 0x0400;
  const int PROCESS_VM_READ = 0x0010;
  const long MEM_COMMIT = 0x1000;
  const long MEM_PRIVATE = 0x20000;
  const int PAGE_NOACCESS = 0x01;
  const int PAGE_GUARD = 0x100;

  static IntPtr Open(int pid) {
    return OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid);
  }

  static List<long[]> EnumerateRegions(IntPtr h, out long committed) {
    var regions = new List<long[]>();
    committed = 0;
    IntPtr addr = IntPtr.Zero;
    var mbi = new MEMORY_BASIC_INFORMATION();
    int mbiSize = Marshal.SizeOf(mbi);
    while (true) {
      if (VirtualQueryEx(h, addr, out mbi, (IntPtr)mbiSize) == IntPtr.Zero) break;
      long b = mbi.BaseAddress.ToInt64(), s = mbi.RegionSize.ToInt64();
      if (s <= 0) break;
      // 只取「私有已提交、可读、非 guard」区域：这些才是堆/私有分配，
      // 排除 image mapping 与 reserve-only（否则会把 DLL 也算进来）。
      if (mbi.State == MEM_COMMIT && mbi.Type == MEM_PRIVATE
          && mbi.Protect != PAGE_NOACCESS && (mbi.Protect & PAGE_GUARD) == 0) {
        regions.Add(new long[]{ b, s, mbi.Protect });
        committed += s;
      }
      addr = (IntPtr)(b + s);
      if (b + s <= b) break;
    }
    return regions;
  }

  static string Band(long bytes) {
    long kb = bytes / 1024;
    if (kb < 64) return "<64KB";
    if (kb < 256) return "64-256KB";
    if (kb < 1024) return "256KB-1MB";
    if (kb < 4096) return "1-4MB";
    if (kb < 8192) return "4-8MB";
    if (kb < 12288) return "8-12MB";
    return ">=12MB";
  }

  static readonly string[] BANDS = { "<64KB","64-256KB","256KB-1MB","1-4MB","4-8MB","8-12MB",">=12MB" };

  // ───────────────────────── 1. 区域枚举 ─────────────────────────
  public static void Regions(int pid, string outFile) {
    var log = new List<string>();
    IntPtr h = Open(pid);
    if (h == IntPtr.Zero) { File.WriteAllText(outFile, "OpenProcess FAILED err=" + Marshal.GetLastWin32Error()); return; }
    long committed;
    var regions = EnumerateRegions(h, out committed);
    CloseHandle(h);

    log.Add("=== PID " + pid + " private regions ===");
    log.Add("privateCommitted = " + MB(committed) + " MB across " + regions.Count + " regions");
    log.Add("");
    log.Add("=== size buckets ===");
    foreach (var b in BANDS) {
      var rs = regions.Where(r => Band(r[1]) == b).ToList();
      if (rs.Count == 0) continue;
      log.Add(string.Format("{0,-12} regions={1,-6} total={2,10} MB", b, rs.Count, MB(rs.Sum(r => r[1]))));
    }
    log.Add("");
    log.Add("=== top 30 regions by size (identical sizes in bulk = repeated large structure) ===");
    int i = 0;
    foreach (var r in regions.OrderByDescending(r => r[1]).Take(30)) {
      i++;
      log.Add(string.Format("#{0,-3} base=0x{1,-14} size={2,10} KB protect=0x{3}",
        i, r[0].ToString("x"), r[1]/1024, r[2].ToString("x")));
    }
    // 同尺寸聚集检测：这是发现「重复大结构」的关键信号
    log.Add("");
    log.Add("=== identical-size clusters (>=3 regions of the exact same size) ===");
    foreach (var g in regions.GroupBy(r => r[1]).Where(g => g.Count() >= 3).OrderByDescending(g => g.Count() * g.Key)) {
      log.Add(string.Format("size={0,10} KB  count={1,-5} total={2,9} MB",
        g.Key/1024, g.Count(), MB(g.Count() * g.Key)));
    }
    File.WriteAllLines(outFile, log, new UTF8Encoding(false));
    Console.WriteLine("written " + outFile);
  }

  // ───────────────────────── 2. 字节构成 ─────────────────────────
  public static void Composition(int pid, string outFile) {
    var log = new List<string>();
    IntPtr h = Open(pid);
    if (h == IntPtr.Zero) { File.WriteAllText(outFile, "OpenProcess FAILED"); return; }
    long committed;
    var regions = EnumerateRegions(h, out committed);

    long ascii = 0, utf8 = 0, zero = 0, ctrl = 0, total = 0;
    var perBand = new Dictionary<string, long[]>();
    foreach (var b in BANDS) perBand[b] = new long[4];

    byte[] buf = new byte[4 * 1024 * 1024];
    foreach (var r in regions) {
      string bn = Band(r[1]);
      long off = 0;
      while (off < r[1]) {
        int want = (int)Math.Min((long)buf.Length, r[1] - off);
        IntPtr read;
        bool ok = ReadProcessMemory(h, (IntPtr)(r[0] + off), buf, want, out read);
        int n = (int)read.ToInt64();
        if (n > 0) {
          total += n;
          for (int i = 0; i < n; i++) {
            byte c = buf[i];
            if (c == 0) { zero++; perBand[bn][2]++; }
            else if (c >= 0x20 && c < 0x7F) { ascii++; perBand[bn][0]++; }
            else if (c >= 0x80) { utf8++; perBand[bn][1]++; }
            else { ctrl++; perBand[bn][3]++; }
          }
        }
        if (!ok && n <= 0) break;
        off += want;
      }
    }
    CloseHandle(h);

    log.Add("=== PID " + pid + " byte composition ===");
    log.Add("committed = " + MB(committed) + " MB   bytesRead = " + MB(total) + " MB");
    log.Add("");
    log.Add("ASCII printable (0x20-7E) = " + MB(ascii) + " MB (" + Pct(ascii, total) + "%)");
    log.Add("UTF-8 multibyte (>=0x80) = " + MB(utf8) + " MB (" + Pct(utf8, total) + "%)   <- 中文文本");
    log.Add("zero (0x00)               = " + MB(zero) + " MB (" + Pct(zero, total) + "%)   <- 容量冗余/已释放未归还");
    log.Add("control other             = " + MB(ctrl) + " MB (" + Pct(ctrl, total) + "%)");
    log.Add("");
    log.Add("=> TEXT     = " + MB(ascii + utf8) + " MB (" + Pct(ascii + utf8, total) + "%)");
    log.Add("=> NON-TEXT = " + MB(zero + ctrl) + " MB (" + Pct(zero + ctrl, total) + "%)");
    log.Add("");
    log.Add("=== per band ===");
    log.Add(string.Format("{0,-12} {1,10} {2,10} {3,10} {4,10}", "band", "asciiMB", "utf8MB", "zeroMB", "ctrlMB"));
    foreach (var b in BANDS) {
      var v = perBand[b];
      if (v[0]+v[1]+v[2]+v[3] == 0) continue;
      log.Add(string.Format("{0,-12} {1,10} {2,10} {3,10} {4,10}", b,
        MB(v[0]), MB(v[1]), MB(v[2]), MB(v[3])));
    }
    log.Add("");
    log.Add("解读：零字节高度集中在 4-12MB 大区域 = 大结构分配 + 分配器容量冗余（Windows 默认堆不归还 OS）。");
    File.WriteAllLines(outFile, log, new UTF8Encoding(false));
    Console.WriteLine("written " + outFile);
  }

  // ───────────────────────── 3. 关键词计数 ─────────────────────────
  public static void Markers(int pid, string outFile) {
    var log = new List<string>();
    IntPtr h = Open(pid);
    if (h == IntPtr.Zero) { File.WriteAllText(outFile, "OpenProcess FAILED"); return; }
    long committed;
    var regions = EnumerateRegions(h, out committed);

    // 默认关键词：按本项目结构命名，可用 --markers 自行扩展
    string[] markers = {
      // Ringing / timeline
      "-conversation-", "-control-", "-tool-", "\"schema\":\"qaqh.Ringing\"", "server_epoch",
      "timeline_seq", "BlockCheckpoint", "ToolProgress", "event_id",
      // 存储
      "messages.jsonl", "tool_outbox", "journal_bytes", "ringing-timeline", "ringing-offload",
      "captured_full", "ExecProgressEvent",
      // 会话 seed（探测哪个会话占大头）
      "0d15f370","0e1057bc","115f99c3","203c8b32","37b6389d","3ff7fc91","548a171b",
      "787711d0","7ccbf258","a604383d","c5e4039f","de0522b9","e992a833","f1aeaa3d",
      "1cfbbdcd","4b73c33b","b529b5a9","caf913f5","6b7c5623",
      // 图片
      "iVBORw0KGgo",
      // 运行时
      "tokio", "axum", "serde_json", "zstd", "tower", "hyper",
    };
    var counts = new Dictionary<string, long>();
    foreach (var m in markers) counts[m] = 0;
    long total = 0;

    byte[] buf = new byte[4 * 1024 * 1024];
    foreach (var r in regions) {
      long off = 0;
      while (off < r[1]) {
        int want = (int)Math.Min((long)buf.Length, r[1] - off);
        IntPtr read;
        bool ok = ReadProcessMemory(h, (IntPtr)(r[0] + off), buf, want, out read);
        int n = (int)read.ToInt64();
        if (n > 0) {
          total += n;
          string t = Encoding.ASCII.GetString(buf, 0, n);
          foreach (var m in markers) {
            int i = 0;
            while ((i = t.IndexOf(m, i, StringComparison.Ordinal)) >= 0) { counts[m]++; i += m.Length; }
          }
        }
        if (!ok && n <= 0) break;
        off += want;
      }
    }
    CloseHandle(h);

    log.Add("=== PID " + pid + " marker occurrences ===");
    log.Add("committed = " + MB(committed) + " MB   bytesRead = " + MB(total) + " MB");
    log.Add("");
    foreach (var kv in counts.OrderByDescending(k => k.Value))
      log.Add(string.Format("  {0,-34} {1,12}", kv.Key, kv.Value));
    log.Add("");
    log.Add("提示：把内存里的计数与磁盘上的计数对比（见 README §4），比值 >>1 说明有非活跃副本/副本放大。");
    File.WriteAllLines(outFile, log, new UTF8Encoding(false));
    Console.WriteLine("written " + outFile);
  }

  // ───────────────────────── 4. 去重分析 ─────────────────────────
  public static void Dup(int pid, string outFile, int minLen) {
    var log = new List<string>();
    IntPtr h = Open(pid);
    if (h == IntPtr.Zero) { File.WriteAllText(outFile, "OpenProcess FAILED"); return; }
    long committed;
    var regions = EnumerateRegions(h, out committed);

    var uniq = new Dictionary<string, long[]>();   // md5 -> [count, len]
    long totalRuns = 0, totalRunBytes = 0;
    byte[] buf = new byte[4 * 1024 * 1024];
    var md5 = System.Security.Cryptography.MD5.Create();

    foreach (var r in regions) {
      long off = 0;
      while (off < r[1]) {
        int want = (int)Math.Min((long)buf.Length, r[1] - off);
        IntPtr read;
        bool ok = ReadProcessMemory(h, (IntPtr)(r[0] + off), buf, want, out read);
        int n = (int)read.ToInt64();
        if (n > 0) {
          string text = Encoding.ASCII.GetString(buf, 0, n);
          int i = 0;
          while (i < text.Length) {
            if (text[i] >= 0x20 && text[i] < 0x7F) {
              int j = i;
              while (j < text.Length && text[j] >= 0x20 && text[j] < 0x7F) j++;
              if (j - i >= minLen) {
                string run = text.Substring(i, j - i);
                totalRuns++; totalRunBytes += run.Length;
                string key = Convert.ToBase64String(md5.ComputeHash(Encoding.ASCII.GetBytes(run)));
                long[] e;
                if (uniq.TryGetValue(key, out e)) e[0]++;
                else uniq[key] = new long[]{ 1, run.Length };
              }
              i = j;
            } else i++;
          }
        }
        if (!ok && n <= 0) break;
        off += want;
      }
    }
    CloseHandle(h);

    long uniqueBytes = 0, dupExtra = 0, dupRuns = 0;
    foreach (var kv in uniq.Values) {
      uniqueBytes += kv[1];
      if (kv[0] > 1) { dupExtra += (kv[0]-1)*kv[1]; dupRuns += kv[0]-1; }
    }

    log.Add("=== PID " + pid + " dedup analysis (minRun=" + minLen + ") ===");
    log.Add("committed        = " + MB(committed) + " MB");
    log.Add("runs found       = " + totalRuns);
    log.Add("total run bytes  = " + MB(totalRunBytes) + " MB");
    log.Add("distinct runs    = " + uniq.Count);
    log.Add("distinct bytes   = " + MB(uniqueBytes) + " MB   <- 唯一内容");
    log.Add("duplicated extra = " + MB(dupExtra) + " MB across " + dupRuns + " extra copies");
    log.Add("");
    log.Add("=== top 20 distinct runs by (count x len) ===");
    foreach (var v in uniq.Values.OrderByDescending(v => v[0]*v[1]).Take(20))
      log.Add(string.Format("x{0,-6} len={1,-9} weight={2,-11}", v[0], v[1], v[0]*v[1]));
    log.Add("");
    log.Add("=== top 15 by single length (largest single strings) ===");
    foreach (var v in uniq.Values.OrderByDescending(v => v[1]).Take(15))
      log.Add(string.Format("len={0,-10} copies={1}", v[1], v[0]));
    File.WriteAllLines(outFile, log, new UTF8Encoding(false));
    Console.WriteLine("written " + outFile);
  }

  // ───────────────────────── 5. 区域抽样 dump ─────────────────────────
  public static void Dump(int pid, string outFile, int sizeLoKB, int sizeHiKB, int howMany, int dumpKB) {
    var log = new List<string>();
    IntPtr h = Open(pid);
    if (h == IntPtr.Zero) { File.WriteAllText(outFile, "OpenProcess FAILED"); return; }
    long committed;
    var regions = EnumerateRegions(h, out committed);

    var sel = regions.Where(r => r[1]/1024 >= sizeLoKB && r[1]/1024 <= sizeHiKB)
                     .OrderByDescending(r => r[1]).Take(howMany).ToList();
    int inBand = regions.Count(r => r[1]/1024 >= sizeLoKB && r[1]/1024 <= sizeHiKB);
    log.Add("=== PID " + pid + " dump " + sel.Count + " regions sized " + sizeLoKB + "-" + sizeHiKB + " KB (band has " + inBand + ") ===");
    log.Add("");

    byte[] buf = new byte[dumpKB * 1024];
    foreach (var r in sel) {
      long b = r[0], s = r[1];
      int want = (int)Math.Min((long)buf.Length, s);
      IntPtr read;
      bool ok = ReadProcessMemory(h, (IntPtr)b, buf, want, out read);
      int n = (int)read.ToInt64();
      log.Add("--- base=0x" + b.ToString("x") + " size=" + (s/1024) + "KB protect=0x" + r[2].ToString("x") + " read=" + n + " ---");
      if (n <= 0) { log.Add("  (unreadable)"); continue; }

      int pr = 0; for (int i = 0; i < n; i++) { byte c = buf[i]; if (c >= 0x20 && c < 0x7F) pr++; }
      log.Add("  printable in window = " + (pr/1024) + " KB / " + (n/1024) + " KB");

      // 指针密度：区分「字符串容器」与「哈希表/树」
      int ptrLike = 0, zero = 0, qtotal = n/8;
      for (int i = 0; i + 8 <= n; i += 8) {
        ulong q = BitConverter.ToUInt64(buf, i);
        if (q == 0) zero++;
        else if (q > 0x10000 && q < 0x7FFFFFFFFFFFUL) ptrLike++;
      }
      log.Add("  pointer density = " + (qtotal > 0 ? (100.0*ptrLike/qtotal).ToString("F1") : "0") + "%   zero qwords = " + zero);

      string t = Encoding.ASCII.GetString(buf, 0, n);
      var runs = new List<string>();
      foreach (System.Text.RegularExpressions.Match m in System.Text.RegularExpressions.Regex.Matches(t, @"[\x20-\x7E]{60,}"))
        runs.Add(m.Value);
      log.Add("  runs>=60: " + runs.Count);
      foreach (var run in runs.OrderByDescending(x => x.Length).Take(4)) {
        string pv = run.Length > 180 ? run.Substring(0,180) + "..." : run;
        log.Add("   [" + run.Length + " ch] " + pv.Replace("\r"," ").Replace("\n"," "));
      }
      if (runs.Count == 0) {
        var sb = new StringBuilder();
        for (int i = 0; i < Math.Min(64, n); i++) sb.Append(buf[i].ToString("x2"));
        log.Add("  hex head: " + sb.ToString());
      }
      log.Add("");
    }
    File.WriteAllLines(outFile, log, new UTF8Encoding(false));
    Console.WriteLine("written " + outFile);
  }

  // ───────────────────────── helpers ─────────────────────────
  static string MB(long bytes) { return (bytes / 1048576.0).ToString("F1"); }
  static string Pct(long part, long whole) { return whole > 0 ? (100.0*part/whole).ToString("F1") : "0"; }
}

Towerminer GUI 0.3.0 for Windows
================================

A simple window for towerminer, the CPU miner of Jetsam (JTM).
Windows 10 or 11, 64-bit.

Files
-----
  towerminer-gui.exe   the window: start this one
  towerminer.exe       the miner (started and stopped by the window)
  README-GUI.txt       this file

Keep both .exe files in the same folder.

Quick start
-----------
1. Extract the whole zip into a folder (right-click the zip > Extract All...).
2. Double-click towerminer-gui.exe.
3. Fill in:
   Node RPC URL      Address of your Jetsam node or pool, for example
                     http://127.0.0.1:9701 for a node running on this PC.
   Mining key        The key your pool gave you (or your node's mining key).
                     It stays hidden, and it is saved only if you tick
                     "Remember key".
   Coinbase address  Optional. Only for your own node started with
                     --allow-custom-coinbase. Leave it empty for a pool.
   Worker name       Optional. A name for this PC that your pool can show
                     in its statistics. Empty: no name is sent (the PC's
                     own name is never sent).
   Threads          How many CPU threads to use. Fewer threads keep the PC
                     more responsive.
   Policy            Hashrate: the fastest profile.
                     Efficiency: the most hashes per watt (cooler, quieter).
4. Click Start. Click Stop to stop mining. Closing the window stops the
   miner too.

What the window shows
---------------------
  State          Mining, Waiting (no work from the node or pool yet), Error,
                 Stopped, Exited.
  Hashrate       Hashes per second, reported by the miner every 5 seconds.
  Height         The block height being mined.
  Uptime         Time since the miner started.
  Found          Blocks found. Accepted / Refused: the node's answer.
                 Unknown: submitted, but no answer came back in time.
  Profile        How the miner runs on this CPU: threads, pads, kernel,
                 backend, memory pages.
  Blocks found   Every block of this session, newest first. Times are UTC.
  Log            The miner's messages (last 500 lines).

About 25 % faster with large pages
----------------------------------
If the Profile shows "Pages: normal" (in yellow), give your Windows account
the "Lock pages in memory" right:
  1. Press Windows+R, type  secpol.msc  and press Enter.
     (Windows Pro, Enterprise and Education. Windows Home does not include
     this tool; the miner still works there, without large pages.)
  2. Open Local Policies > User Rights Assignment > Lock pages in memory.
  3. Click "Add User or Group...", type your Windows user name, click
     "Check Names", then OK and OK.
  4. Sign out and sign back in (or restart the PC), then start mining again.
The Profile then shows "Pages: large" (in green).

Settings
--------
Saved in %APPDATA%\towerminer-gui\config.json when you click Start and when
you close the window.

If "Remember key" is ticked, the key is stored in that file as plain text:
tick it only on a PC that you alone use. Untick it to erase the saved key.

The key is handed to the miner through the TOWERMINER_KEY environment
variable, never on its command line.

Troubleshooting
---------------
- "towerminer.exe was not found": both .exe files must sit in the same
  folder. Extract the whole zip, not only the window program.
- The window does not open, or a message mentions OpenGL: this program needs
  OpenGL 2.0 or newer. Install the graphics driver of your PC (Intel, AMD or
  NVIDIA). towerminer.exe also runs without the window, from a command
  prompt:
      set TOWERMINER_KEY=your-key
      towerminer.exe --rpc http://127.0.0.1:9701
- The miner stops at once: read the red message above the numbers and the
  log at the bottom. Most often the URL or the key is wrong, or the node is
  not running.
- Antivirus: some antivirus programs block every cryptocurrency miner.
  Before allowing these files, check them against the SHA256SUMS file
  published with the release.

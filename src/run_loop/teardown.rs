//! Ending a session: stop the helpers, take down every pane's process tree,
//! settle the crash registry and the snapshot, and give the screen back.

use super::RunState;
use crate::*;

impl RunState<'_> {
    pub(super) fn teardown(self) -> ExitCode {
        let RunState {
            term: _,
            mut out,
            rows: _,
            cols: _,
            size_watch,
            mut windows,
            flash: _,
            pending_sends: _,
            max_depth: _,
            ctl_extra_allow: _,
            ctl_listener,
            ctl_audit,
            active: _,
            scanner: _,
            mouse_on: _,
            selection: _,
            outer_mouse_off: _,
            views: _,
            prompt: _,
            buf: _,
            force_repaint: _,
            last_size_check: _,
            mut board,
            mut bus,
            sidecar_writer,
            last_sidecar_flush: _,
            agents,
            world: _,
            launch,
            mut safety_net,
            renderer: _,
            bell: _,
            wake: _,
            mut keys,
            deliberate_exit,
        } = self;
        let session_job = launch.job;
        let dbg = std::env::var_os("ATRIUM_DEBUG").is_some();
        // Stop reading keys before the caller restores the terminal.
        if let Some(k) = keys.as_mut() {
            k.stop();
        }
        // The upkeep thread first: it must not be stopping builds while the panes
        // are torn down, nor holding the session job's handle when it is closed.
        safety_net.stop_upkeep();
        // Kill process TREES, not processes. `pty.kill()` is SIGKILL to the direct
        // child alone, so everything an agent spawned — MCP servers, language
        // servers, node helpers — outlived atrium and was reparented onto init. That
        // leaked on every clean quit, not just on a signal. See `atrium::reap`.
        let pids: Vec<u32> = windows
            .iter()
            .flat_map(|w| w.panes.iter().map(|p| p.pty.pid()))
            .filter(|p| *p != 0)
            .collect();
        for pid in &pids {
            atrium::reap::term_tree(*pid);
        }
        if !pids.is_empty() {
            std::thread::sleep(atrium::reap::GRACE);
        }
        for pid in &pids {
            atrium::reap::kill_tree(*pid);
        }
        for w in windows.iter_mut() {
            // Still reap the direct child, so it does not linger as a zombie.
            for pane in w.panes.iter_mut() {
                let _ = pane.pty.kill();
            }
        }
        // Do not claim success if something survived: a teardown that can fail
        // silently is how 23 agent processes ended up holding 4.6 GB unnoticed.
        let survivors: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|p| atrium::reap::tree_alive(*p))
            .collect();
        // A CLEAN teardown leaves nothing for the watchdog: emptying the registry
        // before it notices is what makes a normal quit silent.
        //
        // A teardown with survivors is the opposite case, and this used to delete
        // the registry there too — unconditionally, on the one path where atrium had
        // just PROVED that pane groups outlived it. It then printed a warning to a
        // terminal `cleanup_screen` was about to tear down and exited; the watchdog
        // woke on pipe EOF, read a file that no longer existed, got an empty list
        // out of `read_registry`'s `unwrap_or_default`, and killed nothing. The
        // record was destroyed at exactly the moment it was needed.
        //
        // So: survivors mean the registry is REWRITTEN down to just those groups —
        // the watchdog then TERMs and KILLs precisely what is left, and a later
        // `atrium reap` finds the same short list rather than a stale full one. Panes
        // that did die are dropped from it, because a dead pane's pgid can be reused.
        // The panes are gone, so nothing holds the compile pool; remove its FIFO
        // (unix) rather than leave a stale path in the temp directory.
        atrium::buildpool::cleanup();
        match atrium::reap::settle_registry(
            safety_net.registry_path(),
            &survivors,
            trust_mode().policy_label(),
        ) {
            Ok(false) => {}
            Ok(true) => eprintln!(
                "atrium: warning: {} pane process group(s) survived teardown: {:?}",
                survivors.len(),
                survivors
            ),
            // The record could not be settled: say so, with the groups a manual
            // cleanup would need, instead of implying the watchdog has them.
            Err(e) => eprintln!(
                "atrium: warning: could not update the crash registry {}: {e}; surviving pane \
                 process group(s), if any, are not recorded for the watchdog: {:?}",
                safety_net.registry_path().display(),
                survivors
            ),
        }
        // Panes that are genuinely gone leave no stamp behind. A survivor keeps
        // its stamp — that is the marker the sweep needs to collect it later.
        for pid in pids.iter().filter(|p| !survivors.contains(p)) {
            atrium::orphan::unstamp(*pid);
        }
        if dbg {
            eprint!("[atrium-dbg killed]\r\n");
        }
        cleanup_screen(&mut out);
        // Deliver the restore before the process exits and takes the writer thread
        // with it — bounded, so a terminal that never reads can't hold atrium open.
        out.finish(SCREEN_FINISH);
        // A deliberate exit: mark the snapshot closed so the next launch in this
        // project does not offer to resume what the operator chose to end. After the
        // screen is restored, so a failure is readable.
        // Whatever the bus and board changed since the last flush, then wait for the
        // writer: a resume should find the team as it was when the session ended.
        for write in [bus.take_pending_write(), board.take_pending_write()]
            .into_iter()
            .flatten()
        {
            sidecar_writer.submit(write);
        }
        sidecar_writer.finish();
        let settled = if deliberate_exit {
            safety_net.settle_session()
        } else {
            Ok(())
        };
        if let Err(e) = settled {
            eprintln!(
                "atrium: warning: could not mark the session snapshot closed ({e}); the next \
                 launch here may offer to resume this session"
            );
        }
        if dbg {
            eprint!("[atrium-dbg cleaned]\r\n");
        }
        drop(windows);
        if dbg {
            eprint!("[atrium-dbg panes-dropped]\r\n");
        }
        // The session job is static, so nothing drops it: close it here, where the
        // run loop's own job used to go out of scope, so kill-on-close still fires
        // before a fleet's worktree teardown rather than at process exit.
        session_job.close();
        // What is still held drops here, in the order these always dropped when
        // `run()` returned (the reverse of their creation): the key reader
        // stops, the safety net closes the watchdog's pipe, the agent watcher
        // joins, the size watcher stops and the ctl socket goes, last.
        drop(keys);
        drop(safety_net);
        drop(agents);
        drop(ctl_audit);
        drop(size_watch);
        drop(ctl_listener);
        ExitCode::SUCCESS
    }
}

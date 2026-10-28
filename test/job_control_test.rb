# frozen_string_literal: true

require_relative "test_helper"
require "pty"
require "stringio"
require "timeout"
require "sharky"

class JobControlTest < Minitest::Test
  PROMPT = /\xE2\x9D\xAF/n

  def setup
    super
    @env = {
      "HOME" => @tmp,
      "XDG_CONFIG_HOME" => File.join(@tmp, "config"),
      "XDG_DATA_HOME" => File.join(@tmp, "data"),
      "PATH" => "/usr/bin:/bin",
      "TERM" => "xterm-256color",
      "NO_COLOR" => "1"
    }
  end

  def test_read_until_keeps_bytes_after_the_match
    reader = StringIO.new("one two\n")

    assert_equal "one", read_until(reader, /one/)
    assert_equal " two", read_until(reader, /two/)
  end

  def test_background_jobs_listing_and_kill
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("sleep 5 &\r")
      out = read_until(reader, /\[\d+\]/)
      assert_match(/\[\d+\]/, out)
      read_until(reader, PROMPT)

      writer.write("jobs\r")
      out = read_until(reader, /sleep 5/)
      read_until(reader, PROMPT)
      assert_match(/sleep 5/, out)

      writer.write("kill %1\r")
      writer.write("exit\r")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_interactive_pipeline_honors_pipefail
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("set -o pipefail; false | true; printf 'pipe-status:%s\\n' \"$?\"\r")
      output = read_until(reader, /pipe-status:\d\r?\n/)
      assert_match(/pipe-status:1\r?\n/, output)
      read_until(reader, PROMPT)

      writer.write("exit 0\r")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_ctrl_z_stops_and_fg_resumes
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("sleep 5\r")
      sleep 0.2
      writer.write("\x1a")
      out = read_until(reader, /Stopped/)
      assert_match(/Stopped/, out)
      read_until(reader, PROMPT)

      writer.write("fg\r")
      sleep 0.2
      writer.write("\x03")
      writer.write("exit 0\r")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_bg_resumes_stopped_job
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("sleep 5\r")
      sleep 0.2
      writer.write("\x1a")
      assert_match(/Stopped/, read_until(reader, /Stopped/))
      read_until(reader, PROMPT)

      writer.write("bg\r")
      sleep 0.1
      writer.write("jobs\r")
      out = read_until(reader, /Running/)
      read_until(reader, PROMPT)
      assert_match(/Running/, out)

      writer.write("kill %1\r")
      writer.write("exit\r")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_fg_on_missing_job_does_not_crash
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("fg %9\r")
      out = read_until(reader, /no such job/)
      assert_includes out, "no such job"
      read_until(reader, PROMPT)

      writer.write("exit 0\r")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_ctrl_z_immediately_after_enter
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("sleep 5\r\x1a")
      assert_match(/Stopped/, read_until(reader, /Stopped/))
      read_until(reader, PROMPT)

      writer.write("fg\r")
      sleep 0.2
      writer.write("\x03")
      writer.write("exit 0\r")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_background_job_exit_before_jobs
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("true &\r")
      read_until(reader, /\[\d+\]/)
      read_until(reader, PROMPT)
      sleep 0.2

      writer.write("jobs\r")
      out = read_until(reader, /Done/)
      read_until(reader, PROMPT)
      assert_match(/Done/, out)

      writer.write("exit\r")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_closing_pty_sends_hup_to_running_jobs
    assert_job_receives_hup_on_pty_close(stopped: false)
  end

  def test_closing_pty_continues_stopped_jobs_after_hup
    assert_job_receives_hup_on_pty_close(stopped: true)
  end

  def test_reap_records_stopped_background_job
    with_child do |pid|
      table = Sharky::Jobs::Table.new
      job = table.add(command: "sleep 5", pgid: pid, pids: [pid])
      Process.kill("TSTP", -pid)

      Timeout.timeout(5) do
        sleep 0.01 until (table.reap; job.stopped?)
      end

      assert_predicate job, :stopped?
    end
  end

  def test_resume_clears_recorded_stop_status
    with_child do |pid|
      table = Sharky::Jobs::Table.new
      job = table.add(command: "sleep 5", pgid: pid, pids: [pid])
      Process.kill("TSTP", -pid)
      Timeout.timeout(5) do
        sleep 0.01 until (table.reap; job.stopped?)
      end

      table.resume(job)
      Process.kill("TERM", -pid)
      assert_equal 143, table.wait(job)
      assert_predicate job, :done?
    end
  end

  def test_fg_waits_for_remaining_pipeline_members_after_a_child_was_reaped
    exited_pid = fork { exit! 0 }
    Process.wait(exited_pid)
    remaining_pid = Process.spawn(RbConfig.ruby, "-e", "sleep 0.1; exit 7", pgroup: true)
    shell = Sharky::Shell.new(env: @env, stdout: StringIO.new, stderr: StringIO.new)
    shell.jobs.add(command: "true | sleep", pgid: remaining_pid, pids: [exited_pid, remaining_pid])

    assert_equal 7, shell.fg_builtin([])
    assert_raises(Errno::ECHILD) { Process.waitpid(remaining_pid, Process::WNOHANG) }
  ensure
    if remaining_pid
      Process.kill("KILL", remaining_pid) rescue nil
      Process.wait(remaining_pid) rescue nil
    end
  end

  def test_resuming_a_pipeline_preserves_completed_member_status
    stopped_pid = Process.spawn("sleep", "5", pgroup: true)
    exited_pid = fork { exit! 7 }
    table = Sharky::Jobs::Table.new
    job = table.add(command: "sleep | false", pgid: stopped_pid, pids: [stopped_pid, exited_pid])
    Process.kill("STOP", stopped_pid)
    Timeout.timeout(5) do
      loop do
        table.reap
        break if job.stopped? && !File.exist?("/proc/#{exited_pid}")
        sleep 0.01
      end
    end

    assert table.resume(job)
    Process.kill("TERM", stopped_pid)
    assert_equal 7, table.wait(job)
    assert_predicate job, :done?
  ensure
    [stopped_pid, exited_pid].compact.each do |pid|
      Process.kill("KILL", pid) rescue nil
      Process.wait(pid) rescue nil
    end
  end

  def test_fg_default_ignores_a_more_recent_completed_job
    live_pid = Process.spawn(RbConfig.ruby, "-e", "sleep 0.1; exit 7", pgroup: true)
    done_pid = fork { exit! 0 }
    Process.wait(done_pid)
    shell = Sharky::Shell.new(env: @env, stdout: StringIO.new, stderr: StringIO.new)
    shell.jobs.add(command: "running", pgid: live_pid, pids: [live_pid])
    shell.jobs.add(command: "done", pgid: done_pid, pids: [done_pid], state: :done, status: 0)

    assert_equal 7, shell.fg_builtin([])
  ensure
    if live_pid
      Process.kill("KILL", live_pid) rescue nil
      Process.wait(live_pid) rescue nil
    end
  end

  def test_jobs_removes_completed_entries_after_listing_them
    shell = Sharky::Shell.new(env: @env, stdout: StringIO.new, stderr: StringIO.new)
    assert_equal 0, shell.run("true & wait %1")
    shell.stdout = StringIO.new
    assert_equal 0, shell.run("jobs")
    assert_match(/Done/, shell.stdout.string)

    shell.stdout = StringIO.new
    assert_equal 0, shell.run("jobs")
    assert_empty shell.stdout.string
    assert_empty shell.jobs.all
  end

  def test_kill_accepts_numeric_signal
    with_child do |pid|
      shell = Sharky::Shell.new(
        env: { "HOME" => @tmp },
        stdout: StringIO.new,
        stderr: StringIO.new
      )
      shell.jobs.add(command: "sleep 5", pgid: pid, pids: [pid])

      assert_equal 0, shell.kill_builtin(["-9", "%1"])
      _, status = Timeout.timeout(5) { Process.waitpid2(pid) }
      assert_equal Signal.list["KILL"], status.termsig
    end
  end

  def test_compound_background_job_lists_command_and_wait
    shell = Sharky::Shell.new(
      env: { "PATH" => "/usr/bin:/bin", "HOME" => @tmp },
      cwd: @tmp,
      stdout: StringIO.new,
      stderr: StringIO.new
    )
    assert_equal 0, shell.run("if true; then sleep 0.5; fi & jobs", compound: true)
    assert_match(/Running.*if true; then sleep 0\.5; fi/, shell.stdout.string)
    assert_equal 0, shell.run("wait", compound: true)
  end

  private

  def assert_job_receives_hup_on_pty_close(stopped:)
    pgid = nil
    marker = File.join(@tmp, "received-hup")
    ready = File.join(@tmp, "child-ready")
    script = File.join(@tmp, "hup-child.rb")
    File.write(script, <<~RUBY)
      Signal.trap("HUP") { File.write(#{marker.dump}, "HUP"); exit }
      File.write(#{ready.dump}, Process.pid.to_s)
      sleep
    RUBY
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("#{RbConfig.ruby} #{script} &\r")
      output = read_until(reader, /\[\d+\] \d+\r?\n/)
      pgid = output.match(/\[\d+\] (\d+)\r?\n/)[1].to_i
      Timeout.timeout(5) { sleep 0.01 until File.exist?(ready) }
      if stopped
        writer.write("kill -STOP %1\rjobs\r")
        read_until(reader, /Stopped[^\r\n]*hup-child\.rb/)
      end

      writer.close
      reader.close
      assert_equal Signal.list["HUP"], wait_for_exit(pid).termsig
      begin
        Timeout.timeout(2) { sleep 0.01 until File.exist?(marker) }
      rescue Timeout::Error
        flunk "background job did not receive HUP#{stopped ? ' and CONT' : ''} on PTY close"
      end
      assert_equal "HUP", File.read(marker)
    end
  ensure
    Process.kill("KILL", -pgid) rescue nil if pgid
  end

  def with_child
    pid = Process.spawn("sleep", "5", pgroup: true)
    yield pid
  ensure
    if pid
      Process.kill("KILL", -pid) rescue nil
      Process.kill("KILL", pid) rescue nil
      Process.wait(pid) rescue nil
    end
  end

  def with_cli
    reader, writer, pid = PTY.spawn(@env, BIN, "--no-config")
    reader.binmode
    writer.binmode
    yield reader, writer, pid
  ensure
    if pid
      Process.kill("KILL", -pid) rescue nil
      Process.kill("KILL", pid) rescue nil
      Process.wait(pid) rescue nil
    end
    writer.close rescue nil
    reader.close rescue nil
  end

  def wait_for(reader, pattern, timeout: 5)
    read_until(reader, pattern, timeout: timeout)
  end

  def read_until(reader, pattern, timeout: 5)
    @read_buffers ||= {}
    buffer = @read_buffers[reader] ||= +"".b
    Timeout.timeout(timeout) do
      loop do
        if (match = buffer.match(pattern))
          consumed = buffer.byteslice(0, match.end(0))
          @read_buffers[reader] = buffer.byteslice(match.end(0)..) || +"".b
          return consumed
        end
        buffer << reader.readpartial(4096)
      end
    end
  rescue EOFError, Errno::EIO
    @read_buffers[reader] = +"".b
    buffer
  rescue Timeout::Error
    flunk "timed out waiting for #{pattern.inspect}; got #{buffer.inspect}"
  end

  def wait_for_exit(pid, timeout: 5)
    _, status = Timeout.timeout(timeout) { Process.wait2(pid) }
    status
  rescue Timeout::Error
    Process.kill("KILL", -pid) rescue nil
    Process.kill("KILL", pid) rescue nil
    Process.wait(pid) rescue nil
    flunk "CLI did not exit"
  end
end

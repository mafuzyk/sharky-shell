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

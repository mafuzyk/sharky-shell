# frozen_string_literal: true

require_relative "test_helper"
require "pty"
require "timeout"
require "sharky"

class InteractiveTest < Minitest::Test
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

  def test_prompt_echo_and_exit
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("printf '\\x68\\x69\\n'\r")
      output = read_until(reader, /hi[\s\S]*\xE2\x9D\xAF/n)
      assert_includes output, "hi"
      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_ctrl_c_clears_line_and_ctrl_d_exits
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("echo hidden\x03")
      wait_for(reader, PROMPT)
      writer.write("printf '\\x73\\x61\\x66\\x65\\n'\r")
      assert_includes read_until(reader, /safe[\s\S]*\xE2\x9D\xAF/n), "safe"
      writer.write("\x04")
      status = wait_for_exit(pid)
      assert_equal 0, status.exitstatus
      refute_includes @last_output.to_s, "hiddenprintf"
    end
  end

  def test_custom_binding_clears_line
    write_config('Sharky.bind "ctrl-x", :clear_line')
    with_cli(config: true) do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("echo nope\x18")
      writer.write("printf '\\x73\\x61\\x66\\x65\\n'\r")
      assert_includes read_until(reader, /safe[\s\S]*\xE2\x9D\xAF/n), "safe"
      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
      refute_includes @last_output.to_s, "nopeprintf"
    end
  end

  def test_abbreviation_expands_on_space
    write_config('Sharky.abbr "gco", "git checkout"')
    with_cli(config: true) do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("gco ")
      output = read_until(reader, /git checkout/)
      assert_includes output, "git checkout"
      writer.write("\x03")
      wait_for(reader, PROMPT)
      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_relative_file_completion_uses_shell_cwd
    work = File.join(@tmp, "work")
    FileUtils.mkdir_p(work)
    File.write(File.join(work, "unique-completion-marker.txt"), "completion-from-shell-cwd\n")

    with_cli(dir: @tmp) do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("cd work\r")
      wait_for(reader, /work[\s\S]*\xE2\x9D\xAF/n)

      writer.write("cat unique-comp\t")
      read_until(
        reader,
        /unique-completion-marker\.txt/,
        message: "Tab completion should use the Shell cwd instead of the process cwd"
      )
      writer.write("\r")
      output = read_until(reader, /completion-from-shell-cwd[\s\S]*\xE2\x9D\xAF/n)
      assert_includes output, "completion-from-shell-cwd"

      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_multiline_paste_is_preserved_and_executed
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("\e[200~printf a\nprintf b\e[201~")
      output = read_until(reader, /printf b/)
      assert_includes output.dup.force_encoding(Encoding::UTF_8), "⏎"
      writer.write("\r")
      assert_match(/ab/, read_until(reader, /ab[\s\S]*\xE2\x9D\xAF/n))
      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_first_interactive_run_creates_default_config
    path = File.join(@tmp, "config", "sharky", "sharky.rb")
    refute File.exist?(path)

    with_cli(config: true) do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end

    assert File.file?(path)
    assert_equal "", File.read(path)
  end

  def test_no_config_does_not_create_config
    with_cli do |reader, writer, pid|
      wait_for(reader, PROMPT)
      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end

    refute File.exist?(File.join(@tmp, "config", "sharky", "sharky.rb"))
  end

  def test_reload_recolors_prompt
    @env["NO_COLOR"] = nil
    write_config('Sharky.theme = { success: "31" }')
    with_cli(config: true) do |reader, writer, pid|
      first = wait_for(reader, PROMPT)
      assert_includes first, "\e[31m"

      write_config('Sharky.theme = { success: "32" }')
      writer.write("reload\r")
      writer.write("printf '\\x52\\x45\\x4c\\x4f\\x41\\x44\\x44\\x4f\\x4e\\x45\\n'\r")
      second = read_until(reader, /RELOADDONE[\s\S]*\xE2\x9D\xAF/n)
      assert_includes second, "\e[32m"

      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_toml_theme_layout_and_reload
    write_config("Sharky.theme :layout")
    write_theme("layout", layout_theme("layout-one"))

    with_cli(config: true) do |reader, writer, pid|
      wait_for(reader, /layout-one> /)

      write_theme("layout", layout_theme("layout-two"))
      writer.write("reload\r")
      read_until(reader, /layout-two> /)

      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_multiline_prompt_keeps_input_on_last_line
    write_config('Sharky.prompt = ->(_ctx) { "top line\nbottom line\n❯ " }')
    with_cli(config: true) do |reader, writer, pid|
      output = wait_for(reader, /bottom line[\s\S]*\xE2\x9D\xAF/n)
      assert_includes output, "top line"

      writer.write("printf '\\x6d\\x75\\x6c\\x74\\x69\\x2d\\x6f\\x6b\\n'\r")
      assert_includes read_until(reader, "multi-ok"), "multi-ok"
      wait_for(reader, PROMPT)

      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_prompt_preset_simple_renders_both_lines
    write_config("Sharky.prompt_preset :simple")
    with_cli(config: true, dir: @tmp) do |reader, writer, pid|
      output = wait_for(reader, /~\r?\n[^\n]*\xE2\x9D\xAF/n)
      assert_includes output.dup.force_encoding(Encoding::UTF_8), "~"

      writer.write("printf 'preset-ok\\n'\r")
      assert_includes read_until(reader, /preset-ok[\s\S]*\xE2\x9D\xAF/n), "preset-ok"

      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_starship_provider_renders_in_pty
    command = fake_starship
    write_config("Sharky.starship(command: #{command.dump}, timeout: 1)")

    with_cli(config: true, dir: @tmp) do |reader, writer, pid|
      wait_for(reader, /STARSHIP> /)

      writer.write("printf 'starship-ok\\n'\r")
      assert_includes read_until(reader, /starship-ok[\s\S]*STARSHIP> /), "starship-ok"

      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_starship_failure_falls_back_to_theme_layout_in_pty
    command = fake_starship(status: 7)
    write_config("Sharky.starship(command: #{command.dump}, timeout: 1)\nSharky.theme :layout")
    write_theme("layout", layout_theme("layout-after-starship"))

    with_cli(config: true, dir: @tmp) do |reader, writer, pid|
      output = wait_for(reader, /layout-after-starship> /)
      refute_includes output, "config error"

      writer.write("\x04")
      assert_equal 0, wait_for_exit(pid).exitstatus
    end
  end

  def test_process_title_is_sharky
    script = File.join(@tmp, "sleep.rb")
    File.write(script, "sleep 1\n")
    pid = Process.spawn(@env, BIN, "--no-config", script, out: File::NULL, err: File::NULL)
    sleep 0.1

    cmdline = File.binread("/proc/#{pid}/cmdline").split("\0")
    assert_equal "sharky", cmdline.first
  ensure
    Process.kill("KILL", pid) rescue nil
    Process.wait(pid) rescue nil
  end

  def test_terminal_is_restored_after_sigterm
    master, slave = PTY.open
    master.binmode
    before = stty(slave.path)
    pid = Process.spawn(@env, BIN, "--no-config", in: slave, out: slave, err: slave)
    wait_for(master, PROMPT)
    Process.kill("TERM", pid)
    wait_for_exit(pid)
    assert_equal before, stty(slave.path)
  ensure
    Process.kill("KILL", pid) rescue nil
    Process.wait(pid) rescue nil
    master.close rescue nil
    slave.close rescue nil
  end

  private

  def write_config(body)
    dir = File.join(@tmp, "config", "sharky")
    FileUtils.mkdir_p(dir)
    File.write(File.join(dir, "sharky.rb"), body)
  end

  def write_theme(name, body)
    dir = File.join(@tmp, "config", "sharky", "themes")
    FileUtils.mkdir_p(dir)
    File.write(File.join(dir, "#{name}.toml"), body)
  end

  def layout_theme(text)
    <<~TOML
      [prompt]
      separator = "none"
      padding = 0

      [styles]
      arch = "137;180;250"
      ink = "30;30;46"

      [[lines]]
      suffix = "> "

      [[segments]]
      line = 1
      text = "#{text}"
      bg = "arch"
      fg = "ink"
      when = "always"
    TOML
  end

  def fake_starship(status: 0)
    path = File.join(@tmp, "starship")
    File.write(path, "#!/bin/sh\nprintf '%s\\n' 'STARSHIP> '\nexit #{status}\n")
    File.chmod(0o755, path)
    path
  end

  def with_cli(config: false, dir: nil)
    argv = [BIN]
    argv << "--no-config" unless config
    options = dir ? { chdir: dir } : {}
    reader, writer, pid = PTY.spawn(@env, *argv, **options)
    reader.binmode
    writer.binmode
    yield reader, writer, pid
  ensure
    if pid
      Process.kill("KILL", pid) rescue nil
      Process.wait(pid) rescue nil
    end
    writer.close rescue nil
    reader.close rescue nil
  end

  def wait_for(reader, pattern, timeout: 5)
    read_until(reader, pattern, timeout: timeout)
  end

  def read_until(reader, pattern, timeout: 5, message: nil)
    buffer = +"".b
    Timeout.timeout(timeout) do
      loop do
        buffer << reader.readpartial(4096)
        @last_output = buffer.dup
        return buffer if buffer.match?(pattern)
      end
    end
  rescue EOFError, Errno::EIO
    @last_output = buffer.dup
    buffer
  rescue Timeout::Error
    @last_output = buffer.dup
    flunk "#{message || "timed out waiting for #{pattern.inspect}"}; got #{buffer.inspect}"
  end

  def wait_for_exit(pid, timeout: 5)
    _, status = Timeout.timeout(timeout) { Process.wait2(pid) }
    status
  rescue Timeout::Error
    Process.kill("KILL", pid) rescue nil
    Process.wait(pid) rescue nil
    flunk "CLI did not exit"
  end

  def stty(path)
    `stty -g -F #{path}`.strip
  end
end

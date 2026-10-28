require "minitest/autorun"
require "open3"
require "tmpdir"
require "fileutils"

ROOT = File.expand_path("..", __dir__)
BIN = File.join(ROOT, "bin", "sharky")
$LOAD_PATH.unshift(File.join(ROOT, "lib"))

module ShellTestHelpers
  def setup
    @tmp = Dir.mktmpdir("sharky-test-")
  end

  def teardown
    FileUtils.remove_entry(@tmp) if @tmp && File.exist?(@tmp)
  end

  def run_sharky(*argv, stdin: "", env: {})
    base = { "HOME" => @tmp, "PATH" => ENV["PATH"], "TERM" => "xterm-256color" }
    Open3.capture3(base.merge(env), BIN, *argv, stdin_data: stdin)
  end
end

Minitest::Test.include(ShellTestHelpers)

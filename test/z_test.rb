require_relative "test_helper"
require "sharky"
require "stringio"

class ZTest < Minitest::Test
  class FakeTerminal
    attr_reader :input, :output

    def initialize(input, output)
      @input = input
      @output = output
    end

    def tty?
      true
    end

    def with_raw
      yield
    end

    def width(text)
      text.to_s.length
    end

    def size
      [80, 24]
    end
  end

  def test_record_and_query_by_fragment
    path = File.join(@tmp, "z.jsonl")
    z = Sharky::Z.new(path: path)
    a = File.join(@tmp, "projects", "alpha")
    b = File.join(@tmp, "projects", "beta")
    FileUtils.mkdir_p(a)
    FileUtils.mkdir_p(b)
    z.record(a)
    z.record(a)
    z.record(b)
    assert_equal a, z.query(["alpha"])
    assert_equal a, z.query(["proj", "alp"])
    assert_equal a, z.query(["ALPHA"])
    assert_nil z.query(["missing"])
    assert_equal 0600, File.stat(path).mode & 07777
  end

  def test_compaction_keeps_best_entries
    path = File.join(@tmp, "z.jsonl")
    z = Sharky::Z.new(path: path)
    3.times { z.record(File.join(@tmp, "a")) }
    z.record(File.join(@tmp, "b"))
    z.compact!(limit: 2)
    assert_operator File.readlines(path).size, :<=, 2
    assert_equal File.join(@tmp, "a"), z.query(["a"])
    assert_equal 0600, File.stat(path).mode & 07777
  end

  def test_default_path_honors_environment
    custom = File.join(@tmp, "custom.jsonl")
    data = File.join(@tmp, "data")

    assert_equal custom, Sharky::Z.default_path({ "SHARKY_Z" => custom })
    assert_equal File.join(data, "sharky", "z.jsonl"),
                 Sharky::Z.default_path({ "XDG_DATA_HOME" => data })
    assert_nil Sharky::Z.default_path({}, fallback: false)
  end

  def test_data_directory_is_private
    directory = File.join(@tmp, "data", "sharky")
    FileUtils.mkdir_p(directory)
    File.chmod(0755, directory)
    z = Sharky::Z.new(path: File.join(directory, "z.jsonl"))

    z.record(File.join(@tmp, "project"))

    assert_equal 0700, File.stat(directory).mode & 07777
  end

  def test_shell_records_successful_cd
    path = File.join(@tmp, "z.jsonl")
    z = Sharky::Z.new(path: path)
    target = File.join(@tmp, "visited")
    Dir.mkdir(target)
    shell = Sharky::Shell.new(
      env: { "HOME" => @tmp, "PATH" => "/usr/bin:/bin" },
      cwd: @tmp,
      stdout: StringIO.new,
      stderr: StringIO.new,
      stdin: StringIO.new,
      z: z
    )

    assert_equal 0, shell.cd(target)
    assert_equal target, z.query([])
    assert_equal 1, File.readlines(path).size
  end

  def test_z_builtin_jumps_to_match
    path = File.join(@tmp, "z.jsonl")
    z = Sharky::Z.new(path: path)
    target = File.join(@tmp, "projects", "alpha")
    FileUtils.mkdir_p(target)
    z.record(target)
    shell = Sharky::Shell.new(
      env: { "HOME" => @tmp, "PATH" => "/usr/bin:/bin" },
      cwd: @tmp,
      stdout: StringIO.new,
      stderr: StringIO.new,
      stdin: StringIO.new,
      z: z
    )

    assert_equal 0, shell.run("z alpha")
    assert_equal target, shell.cwd
  end

  def test_z_builtin_handles_home_and_oldpwd
    work = File.join(@tmp, "work")
    Dir.mkdir(work)
    shell = Sharky::Shell.new(
      env: { "HOME" => @tmp, "PATH" => "/usr/bin:/bin" },
      cwd: work,
      stdout: StringIO.new,
      stderr: StringIO.new,
      stdin: StringIO.new,
      z: Sharky::Z.new(path: File.join(@tmp, "z.jsonl"))
    )

    assert_equal 0, shell.run("z")
    assert_equal @tmp, shell.cwd
    assert_equal 0, shell.run("z -")
    assert_equal work, shell.cwd
  end

  def test_zi_without_terminal_jumps_to_best_match
    z = Sharky::Z.new(path: File.join(@tmp, "z.jsonl"))
    target = File.join(@tmp, "projects", "beta")
    FileUtils.mkdir_p(target)
    z.record(target)
    shell = Sharky::Shell.new(
      env: { "HOME" => @tmp, "PATH" => "/usr/bin:/bin" },
      cwd: @tmp,
      stdout: StringIO.new,
      stderr: StringIO.new,
      stdin: StringIO.new,
      z: z
    )

    assert_equal 0, shell.run("zi beta")
    assert_equal target, shell.cwd
  end

  def test_zi_uses_pager_when_terminal_is_available
    z = Sharky::Z.new(path: File.join(@tmp, "z.jsonl"))
    target = File.join(@tmp, "projects", "gamma")
    FileUtils.mkdir_p(target)
    z.record(target)
    reader, writer = IO.pipe
    output = StringIO.new
    writer.write("\r")
    shell = Sharky::Shell.new(
      env: { "HOME" => @tmp, "PATH" => "/usr/bin:/bin" },
      cwd: @tmp,
      stdout: StringIO.new,
      stderr: StringIO.new,
      stdin: reader,
      terminal: FakeTerminal.new(reader, output),
      z: z
    )

    assert_equal 0, shell.run("zi gamma")
    assert_equal target, shell.cwd
    assert_includes output.string, "z"
  ensure
    writer&.close unless writer&.closed?
    reader&.close unless reader&.closed?
  end
end

# frozen_string_literal: true

require_relative "test_helper"
require "sharky"
require "stringio"

class HistoryTest < Minitest::Test
  def test_add_load_search_and_escape
    path = File.join(@tmp, "history")
    h = Sharky::History.new(path: path)
    h.add("echo a\\b")
    h.add("printf 'x\ny'")
    h.add("echo a\\b")
    h.save

    loaded = Sharky::History.new(path: path).load
    assert_equal ["echo a\\b", "printf 'x\ny'"], loaded.entries
    assert_equal ["echo a\\b"], loaded.search("echo")
    assert_equal 0600, File.stat(path).mode & 07777
  end

  def test_clear
    path = File.join(@tmp, "history")
    h = Sharky::History.new(path: path)
    h.add("one")
    h.clear
    assert_equal [], h.entries
  end

  def test_clear_persists_an_empty_file
    path = File.join(@tmp, "history")
    h = Sharky::History.new(path: path)
    h.add("one")
    h.save
    h.clear
    assert_equal "", File.read(path)
    assert_equal [], Sharky::History.new(path: path).load.entries
  end

  def test_add_ignores_empty_and_consecutive_duplicates
    path = File.join(@tmp, "history")
    h = Sharky::History.new(path: path)
    h.add("one")
    h.add("one")
    h.add("")
    h.add("two")
    assert_equal ["one", "two"], h.entries
  end

  def test_save_appends_only_new_entries
    path = File.join(@tmp, "history")
    h = Sharky::History.new(path: path)
    h.add("one")
    h.save
    h.add("two")
    h.save

    assert_equal ["one", "two"], Sharky::History.new(path: path).load.entries
    assert_equal 2, File.readlines(path).length
  end

  def test_load_ignores_blank_lines
    path = File.join(@tmp, "history")
    File.write(path, "one\n\ntwo\n")
    assert_equal ["one", "two"], Sharky::History.new(path: path).load.entries
  end

  def test_search_returns_most_recent_first
    path = File.join(@tmp, "history")
    h = Sharky::History.new(path: path)
    h.add("echo one")
    h.add("ls")
    h.add("echo two")
    assert_equal ["echo two", "echo one"], h.search("echo")
    assert_equal ["echo two", "ls", "echo one"], h.search("")
  end

  def test_directory_and_file_permissions
    path = File.join(@tmp, "data", "sharky", "history")
    h = Sharky::History.new(path: path)
    h.add("one")
    h.save
    assert_equal 0700, File.stat(File.dirname(path)).mode & 07777
    assert_equal 0600, File.stat(path).mode & 07777
  end

  def test_default_path_honors_sharky_history
    old = ENV["SHARKY_HISTORY"]
    ENV["SHARKY_HISTORY"] = File.join(@tmp, "custom-history")
    assert_equal File.join(@tmp, "custom-history"), Sharky::History.new.path
  ensure
    ENV["SHARKY_HISTORY"] = old
  end

  def test_history_builtin_lists_and_clears
    path = File.join(@tmp, "history")
    h = Sharky::History.new(path: path)
    h.add("echo hi")
    h.save
    shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new, history: h)
    shell.run("history")
    assert_includes shell.stdout.string, "echo hi"
    shell.run("history -c")
    assert_equal [], h.entries
  end
end

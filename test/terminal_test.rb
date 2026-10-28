# frozen_string_literal: true

require_relative "test_helper"
require "pty"
require "sharky"

class TerminalTest < Minitest::Test
  def test_width_counts_wide_and_combining_characters
    terminal = Sharky::Terminal.new(input: StringIO.new, output: StringIO.new)
    assert_equal 4, terminal.width("日本")
    assert_equal 1, terminal.width("e\u0301")
    assert_equal 0, terminal.width("")
  end

  def test_pty_round_trip_preserves_terminal_mode
    master, slave = PTY.open
    terminal = Sharky::Terminal.new(input: slave, output: slave)
    assert terminal.tty?
    before = terminal_mode(slave)
    terminal.with_raw do
      assert terminal.tty?
    end
    assert_equal before, terminal_mode(slave)
    terminal.restore
    assert terminal.tty?
  ensure
    master.close rescue nil
    slave.close rescue nil
  end

  def test_with_raw_restores_after_exception
    master, slave = PTY.open
    terminal = Sharky::Terminal.new(input: slave, output: slave)
    before = terminal_mode(slave)
    assert_raises(RuntimeError) do
      terminal.with_raw { raise "boom" }
    end
    assert_equal before, terminal_mode(slave)
  ensure
    master.close rescue nil
    slave.close rescue nil
  end

  def test_size_is_columns_then_rows
    master, slave = PTY.open
    slave.winsize = [24, 80]
    terminal = Sharky::Terminal.new(input: slave, output: slave)
    assert_equal [80, 24], terminal.size
  ensure
    master.close rescue nil
    slave.close rescue nil
  end

  def test_job_control_methods_degrade_without_a_terminal
    terminal = Sharky::Terminal.new(input: StringIO.new, output: StringIO.new)
    assert_nil terminal.foreground_pgrp
    refute terminal.give_terminal(Process.getpgrp)
    refute terminal.take_terminal
  end

  private

  def terminal_mode(io)
    `stty -g -F #{io.path}`.strip
  end
end

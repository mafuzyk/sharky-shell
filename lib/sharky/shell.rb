# frozen_string_literal: true

module Sharky
  class Shell
    attr_reader :env, :cwd, :terminal, :history, :jobs, :functions, :positional, :script_name
    attr_accessor :stdin, :stdout, :stderr
    attr_writer :runtime
    attr_reader :last_status, :last_bg_pid, :shell_pgid, :foreground_pgid
    attr_accessor :pending_signal, :reap_requested, :redraw_requested

    def initialize(env: ENV.to_h, cwd: Dir.pwd, stdin: $stdin, stdout: $stdout, stderr: $stderr, terminal: nil,
                   history: nil, runtime: nil, z: nil, positional: [], script_name: "sharky")
      @env = env.to_h.dup
      @cwd = File.expand_path(cwd)
      @positional = Array(positional).map(&:to_s)
      @script_name = script_name.to_s
      @positional_stack = []
      @stdin = stdin
      @stdout = stdout
      @stderr = stderr
      @terminal = terminal
      @history = history
      @runtime = runtime
      @z = z
      @z_loaded = !z.nil?
      @jobs = Jobs::Table.new
      @functions = {}
      @last_status = 0
      @last_bg_pid = nil
      @exit_requested = false
      @shell_pgid = Process.getpgrp
      @foreground_pgid = nil
      @job_control = false
      @job_control_warned = false
      @pending_signal = nil
      @reap_requested = false
      @redraw_requested = false
      @env["$"] ||= Process.pid.to_s
      @env["?"] = "0"
      @env["PWD"] = @cwd
    end

    def enable_job_control
      return @job_control if @job_control
      return false unless @terminal&.tty?

      begin
        Process.setpgid(0, 0) if Process.getpgrp != Process.pid
        @shell_pgid = Process.getpgrp
      rescue SystemCallError => error
        disable_job_control(error.message)
        return false
      end

      unless @terminal.take_terminal
        disable_job_control("cannot take the terminal")
        return false
      end

      @job_control = true
    end

    def job_control?
      @job_control
    end

    def run(source, compound: false)
      program = Parser.parse(source, compound: compound)
      run_program(program)
    rescue ParseError => error
      report_parse_error(source, error)
      mark_status(2)
    rescue Expansion::ParameterError => error
      @stderr.puts("sharky: #{error.message}")
      mark_status(1)
    rescue Arithmetic::Error => error
      @stderr.puts("sharky: #{error.message}")
      mark_status(1)
    rescue ControlFlow::Break
      @stderr.puts("sharky: break: only meaningful in a loop")
      mark_status(1)
    rescue ControlFlow::Continue
      @stderr.puts("sharky: continue: only meaningful in a loop")
      mark_status(1)
    rescue ControlFlow::Return
      @stderr.puts("sharky: return: only meaningful in a function")
      mark_status(1)
    ensure
      reap_jobs
    end

    def report_parse_error(source, error)
      pos = error.pos
      if pos
        pos = pos.clamp(0, source.length)
        line_start = pos.zero? ? 0 : (source.rindex("\n", pos - 1) || -1) + 1
        line_end = source.index("\n", pos) || source.length
        line = source[line_start...line_end]
        caret = "#{line[0...pos - line_start].gsub(/[^\t]/, " ")}^"
        @stderr.puts("sharky: #{line}")
        @stderr.puts("sharky: #{caret}")
      end
      @stderr.puts("sharky: #{error.message}")
    end

    def run_program(program, foreground: true)
      status = Executor.new(self).execute(program, foreground: foreground)
      mark_status(status)
    end

    def cd(path)
      target = path.nil? || path.empty? ? home : path
      target = @env["OLDPWD"] || @cwd if target == "-"
      expanded = File.expand_path(target, @cwd)
      unless File.directory?(expanded)
        @stderr.puts("sharky: cd: #{target}: No such file or directory")
        return 1
      end

      @env["OLDPWD"] = @cwd
      @cwd = expanded
      @env["PWD"] = @cwd
      record_visit(@cwd)
      0
    end

    def z
      return @z if @z_loaded

      @z_loaded = true
      path = Z.default_path(@env, fallback: false)
      @z = path && Z.new(path: path)
    end

    def export(name, value = nil)
      @env[name] = value.nil? ? (@env[name] || "") : value
    end

    def unset(name)
      @env.delete(name)
    end

    def last_bg_pid=(pid)
      @last_bg_pid = pid
      @env["!"] = pid.to_s
    end

    def request_exit(status)
      @exit_requested = true
      mark_status(status)
    end

    def exit_requested?
      @exit_requested
    end

    def mark_status(status)
      @last_status = status
      @env["?"] = status.to_s
      status
    end

    def runtime
      @runtime ||= Runtime.new
    end

    def expansion
      Expansion.new(env: @env, positional: @positional, script_name: @script_name, cwd: @cwd)
    end

    def with_positional(values)
      @positional_stack << @positional
      @positional = Array(values).map(&:to_s)
      yield
    ensure
      @positional = @positional_stack.pop || []
    end

    def shift(count = 1)
      count = Integer(count, 10) if count.is_a?(String)
      count = count.to_i
      return 1 if count.negative? || count > @positional.length

      @positional.shift(count)
      0
    rescue ArgumentError
      1
    end

    def track_foreground(pgid)
      @foreground_pgid = pgid
    end

    def give_terminal(pgid)
      return false unless job_control?

      @foreground_pgid = pgid
      @terminal.give_terminal(pgid)
    end

    def take_terminal
      @foreground_pgid = nil
      @pending_signal = nil
      @terminal&.take_terminal if job_control?
    end

    def forward_pending_signal
      signal = @pending_signal
      return false unless signal && @foreground_pgid

      @pending_signal = nil
      attempts = signal == "TSTP" ? 20 : 1
      attempts.times do
        Process.kill(signal, -@foreground_pgid)
        sleep 0.005 if signal == "TSTP"
      end
      true
    rescue SystemCallError
      false
    end

    def clear_pending_signal
      @pending_signal = nil
    end

    def reap_jobs
      @jobs.reap
      @reap_requested = false
    end

    def notify_jobs
      @jobs.all.each do |job|
        next if job.running? || job.notified

        marker = job.equal?(@jobs.current) ? "+" : "-"
        state = job.stopped? ? "Stopped" : "Done"
        @stdout.puts("[#{job.id}]#{marker} #{state} #{job.command}")
        job.notified = true
      end
    end

    def jobs_builtin(_argv)
      reap_jobs
      @jobs.all.each do |job|
        state = case job.state
                when :running then "Running"
                when :stopped then "Stopped"
                else "Done"
                end
        @stdout.puts("[#{job.id}] #{state} #{job.pgid} #{job.command}")
        job.notified = true unless job.running?
      end
      0
    end

    def fg_builtin(argv)
      target = argv.first
      job = target ? @jobs.find(target) : @jobs.current
      return no_such_job("fg", target) unless job
      if job.done?
        @stderr.puts("sharky: fg: #{job.command}: job has terminated")
        return 1
      end

      resume_job(job, foreground: true)
    end

    def bg_builtin(argv)
      target = argv.first
      job = target ? @jobs.find(target) : @jobs.current
      return no_such_job("bg", target) unless job
      if job.done?
        @stderr.puts("sharky: bg: #{job.command}: job has terminated")
        return 1
      end

      @jobs.resume(job)
      0
    end

    def wait_builtin(argv)
      if argv.empty?
        @jobs.all.each { |job| @jobs.wait(job) unless job.done? || job.stopped? }
        return 0
      end

      target = argv.first
      job = @jobs.find(target) if target.start_with?("%")
      job ||= @jobs.all.find { |candidate| candidate.pids.include?(target.to_i) }
      return @jobs.wait(job) if job

      pid = Integer(target, 10)
      _, status = Process.waitpid2(pid)
      decode_wait_status(status)
    rescue Errno::ECHILD, ArgumentError
      @stderr.puts("sharky: wait: #{target}: no such process")
      127
    end

    def kill_builtin(argv)
      return usage_error("kill", "usage: kill [-SIGNAL] %job|pid ...") if argv.empty?

      args = argv.dup
      signal = "TERM"
      if args.first.start_with?("-")
        name = args.shift.delete_prefix("-")
        signal = signal_name(name)
        return usage_error("kill", "invalid signal: #{name}") unless signal
      end

      status = 0
      args.each do |target|
        if target.start_with?("%")
          job = @jobs.find(target)
          unless job
            @stderr.puts("sharky: kill: #{target}: no such job")
            status = 1
            next
          end
          Process.kill(signal, -job.pgid)
        else
          pid = Integer(target, 10)
          Process.kill(signal, pid)
        end
      rescue SystemCallError, ArgumentError
        @stderr.puts("sharky: kill: #{target}: no such process")
        status = 1
      end
      status
    end

    private

    def resume_job(job, foreground:)
      @jobs.resume(job) if job.stopped?
      return 0 unless foreground

      give_terminal(job.pgid)
      forward_pending_signal
      status = @jobs.wait(job)
      take_terminal
      notify_jobs if job.stopped?
      status
    end

    def no_such_job(command, target)
      @stderr.puts("sharky: #{command}: #{target}: no such job")
      1
    end

    def usage_error(command, message)
      @stderr.puts("sharky: #{command}: #{message}")
      1
    end

    def signal_name(name)
      return nil if name.empty?

      key = name.upcase.delete_prefix("SIG")
      return key.to_i if key.match?(/\A\d+\z/) && Signal.list.value?(key.to_i)

      Signal.list.key?(key) ? key : nil
    end

    def decode_wait_status(status)
      return 128 + status.termsig if status.signaled?

      status.exitstatus || 1
    end

    def disable_job_control(reason)
      return false if @job_control_warned

      @job_control_warned = true
      @stderr.puts("sharky: job control disabled: #{reason}")
      false
    end

    def record_visit(path)
      z&.record(path)
    rescue SystemCallError, EncodingError
      nil
    end

    def home
      @env["HOME"] || Dir.home
    end
  end
end

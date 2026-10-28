# frozen_string_literal: true

module Sharky
  class Repl
    class Context
      def initialize(shell)
        @shell = shell
      end

      def sh(source)
        Sharky.sh(source)
      end

      def capture(source)
        Sharky.capture(source)
      end

      def cd(path = nil)
        status = @shell.cd(path)
        if status.zero?
          Dir.chdir(@shell.cwd)
          ENV["PWD"] = @shell.cwd
        end
        status
      end

      def exit(status = 0)
        raise SystemExit, status
      end
    end

    def self.start(shell, argv: [])
      runtime = shell.runtime
      previous_runtime = Sharky.runtime
      previous_shell = runtime.shell
      previous_dir = Dir.pwd
      previous_env = ENV.to_h
      previous_argv = ARGV.dup

      Sharky.runtime = runtime
      runtime.shell = shell
      ARGV.replace(argv)
      Dir.chdir(shell.cwd)
      ENV.replace(shell.env)

      context = Context.new(shell)
      runner = -> { run(context, shell, argv) }
      if shell.terminal && shell.stdin.respond_to?(:tty?) && shell.stdin.tty?
        shell.terminal.with_cooked { runner.call }
      else
        runner.call
      end
    rescue SystemExit => error
      error.status
    ensure
      ENV.each { |name, value| shell.env[name] = value } if shell&.env
      begin
        shell.cd(Dir.pwd) if shell && Dir.pwd != shell.cwd
      rescue StandardError
        nil
      end
      ENV.replace(previous_env) if previous_env
      begin
        Dir.chdir(previous_dir) if previous_dir
      rescue StandardError
        nil
      end
      ARGV.replace(previous_argv) if previous_argv
      runtime.shell = previous_shell if runtime
      Sharky.runtime = previous_runtime
    end

    def self.run(context, shell, argv)
      if shell.stdin.respond_to?(:tty?) && shell.stdin.tty?
        begin
          return start_irb(context, argv) if require_irb
        rescue LoadError
          nil
        end
      end

      start_fallback(context, shell)
    end

    def self.require_irb
      require "rubygems"
      gem "irb"
      require "irb"
      true
    rescue LoadError, Gem::LoadError
      false
    end
    private_class_method :require_irb

    def self.start_irb(context, argv)
      IRB.setup(nil, argv: argv)
      workspace = IRB::WorkSpace.new(context)
      irb = IRB::Irb.new(workspace)
      IRB.conf[:MAIN_CONTEXT] = irb.context
      irb.run(IRB.conf)
      0
    end
    private_class_method :start_irb

    def self.start_fallback(context, shell)
      input = shell.stdin
      output = shell.stdout
      prompt = shell.stdin.respond_to?(:tty?) && shell.stdin.tty?

      loop do
        line = if prompt
                 require "reline"
                 Reline.readline("sharky-ruby> ", true)
               else
                 input.gets
               end
        break if line.nil?

        code = line.chomp
        next if code.strip.empty?

        begin
          value = context.instance_eval(code, "(sharky-ruby)", 1)
          output.puts("=> #{value.inspect}") unless value.nil?
        rescue SystemExit => error
          return error.status
        rescue Interrupt
          output.puts
        rescue StandardError, ScriptError => error
          output.puts("#{error.class}: #{error.message}")
        end
        output.flush if output.respond_to?(:flush)
      end
      0
    end
    private_class_method :start_fallback

    private_class_method :run
  end
end

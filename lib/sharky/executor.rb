require "shellwords"
require "tempfile"

module Sharky
  class Executor
    def initialize(shell)
      @shell = shell
    end

    def execute(program, foreground: true)
      status = 0
      program.lines.each do |line|
        status = execute_list(line.list, foreground: foreground)
        break if @shell.exit_requested?
      end
      status
    end

    private

    def execute_list(list, foreground:, errexit: true)
      previous_suppression = @shell.errexit_suppressed
      @shell.errexit_suppressed = true unless errexit
      status = 0
      list.entries.each do |kind, and_ors|
        and_or = and_ors.first
        next unless and_or

        status = kind == :async ? start_async(and_or) : execute_and_or(and_or, foreground: foreground)
        break if @shell.exit_requested?
      end
      status
    ensure
      @shell.errexit_suppressed = previous_suppression
    end

    def execute_and_or(and_or, foreground:)
      rest = Array(and_or.rest)
      status = execute_pipeline(and_or.first, foreground: foreground, errexit: rest.empty?)
      return status if @shell.exit_requested?

      rest.each_with_index do |(operator, pipeline), index|
        next if operator == :and && status != 0
        next if operator == :or && status.zero?

        status = execute_pipeline(pipeline, foreground: foreground, errexit: index == rest.length - 1)
        break if @shell.exit_requested?
      end
      status
    end

    def execute_pipeline(pipeline, foreground:, errexit: true)
      previous_suppression = @shell.errexit_suppressed
      @shell.errexit_suppressed = true if !errexit || pipeline.is_a?(AST::Not)
      status = if !pipeline.is_a?(AST::Pipeline)
                 execute_command(pipeline, foreground: foreground)
               elsif pipeline.commands.length == 1
                 execute_command(pipeline.commands.first, foreground: foreground)
               else
                 execute_pipeline_commands(pipeline.commands, foreground: foreground)
               end
      if @shell.option?(:errexit) && !@shell.errexit_suppressed && !status.zero?
        @shell.request_exit(status)
      end
      @shell.mark_status(status)
    ensure
      @shell.errexit_suppressed = previous_suppression
    end

    def execute_command(command, foreground:)
      status = case command
      when AST::Simple
        execute_simple(command, foreground: foreground)
      when AST::Subshell
        execute_subshell(command, foreground: foreground)
      when AST::Redirected
        execute_redirected(command, foreground: foreground)
      when AST::If
        execute_if(command, foreground: foreground)
      when AST::For
        execute_for(command, foreground: foreground)
      when AST::While
        execute_while(command, foreground: foreground)
      when AST::Group
        execute_list(command.list, foreground: foreground)
      when AST::Not
        execute_not(command, foreground: foreground)
      when AST::Arithmetic
        execute_arithmetic(command)
      when AST::TestCommand
        execute_test(command)
      when AST::Case
        execute_case(command, foreground: foreground)
      when AST::FunctionDef
        execute_function_definition(command)
      else
        unsupported
      end
      @shell.mark_status(status)
    end

    def execute_if(command, foreground:)
      command.branches.each do |condition, body|
        return execute_list(body, foreground: foreground) if execute_list(condition, foreground: foreground, errexit: false).zero?
      end
      command.else_body ? execute_list(command.else_body, foreground: foreground) : 0
    end

    def execute_for(command, foreground:)
      words = @shell.expansion.expand_words(command.words)
      status = 0

      words.each do |value|
        @shell.env[command.name] = value
        begin
          status = execute_list(command.body, foreground: foreground)
        rescue ControlFlow::Continue => signal
          signal.levels -= 1
          raise if signal.levels.positive?

          next
        rescue ControlFlow::Break => signal
          signal.levels -= 1
          raise if signal.levels.positive?

          break
        end
        break if @shell.exit_requested?
      end

      status
    end

    def execute_while(command, foreground:)
      status = 0
      loop do
        condition = execute_list(command.condition, foreground: foreground, errexit: false)
        break if command.until_form ? condition.zero? : !condition.zero?
        break if @shell.exit_requested?

        begin
          status = execute_list(command.body, foreground: foreground)
        rescue ControlFlow::Continue => signal
          signal.levels -= 1
          raise if signal.levels.positive?

          next
        rescue ControlFlow::Break => signal
          signal.levels -= 1
          raise if signal.levels.positive?

          break
        end
        break if @shell.exit_requested?
      end
      status
    end

    def execute_not(command, foreground:)
      status = execute_pipeline(command.command, foreground: foreground)
      status.zero? ? 1 : 0
    end

    def execute_arithmetic(command)
      value = Arithmetic.evaluate(command.source, env: @shell.env)
      @shell.mark_status(value.zero? ? 1 : 0)
    end

    def execute_test(command)
      tokens = Lexer.new(command.source).tokenize.filter_map do |token|
        case token.type
        when :word then token.value
        when :and_if then "&&"
        when :or_if then "||"
        when :eof then nil
        else raise ArgumentError
        end
      end
      @shell.mark_status(test_expression(tokens) ? 0 : 1)
    rescue ArgumentError, LexError
      @shell.stderr.puts("sharky: [[: syntax error: bad expression")
      @shell.mark_status(2)
    end

    def test_expression(tokens)
      ors = split_test_logical(tokens, "||")
      return ors.any? { |part| test_and(part) } if ors.length > 1
      test_and(tokens)
    end

    def test_and(tokens)
      ands = split_test_logical(tokens, "&&")
      return ands.all? { |part| test_unary(part) } if ands.length > 1
      test_unary(tokens)
    end

    def test_unary(tokens)
      raise ArgumentError if tokens.empty?

      return !test_unary(tokens[1..]) if word_text(tokens.first) == "!"

      expanded = tokens.map { |word| expand_test_word(word) }
      words = expanded.map(&:first)

      operator = words.first
      operand = words[1..]&.join(" ")
      return words.first.to_s.length.positive? if words.length == 1
      return !operand.to_s.empty? if operator == "-n"
      return operand.to_s.empty? if operator == "-z"
      return File.exist?(test_path(operand)) if %w[-e -a].include?(operator)
      return File.file?(test_path(operand)) if operator == "-f"
      return File.directory?(test_path(operand)) if operator == "-d"
      return File.readable?(test_path(operand)) if operator == "-r"
      return File.writable?(test_path(operand)) if operator == "-w"
      return File.executable?(test_path(operand)) if operator == "-x"

      left = words[0]
      op = words[1]
      right = words[2..]&.join(" ")
      case op
      when "==", "!="
        raise ArgumentError unless expanded.length == 3

        matched = File.fnmatch(expanded[2].last, left, File::FNM_DOTMATCH)
        op == "==" ? matched : !matched
      when "=~" then Regexp.new(right).match?(left)
      when "-eq" then left.to_i == right.to_i
      when "-ne" then left.to_i != right.to_i
      when "-lt" then left.to_i < right.to_i
      when "-le" then left.to_i <= right.to_i
      when "-gt" then left.to_i > right.to_i
      when "-ge" then left.to_i >= right.to_i
      else !left.to_s.empty?
      end
    rescue RegexpError
      false
    end

    def expand_test_word(word)
      text = +""
      pattern = +""
      word.parts.each do |part|
        value = @shell.expansion.expand_assignment(AST::Word.new(parts: [part]))
        text << value
        pattern << if [:single, :double, :escaped].include?(part.type)
                     value.gsub(/[\\*?\[\]]/) { |char| "\\#{char}" }
                   else
                     value
                   end
      end
      [text, pattern]
    end

    def split_test_logical(tokens, operator)
      parts = []
      current = []
      tokens.each do |token|
        if token == operator
          parts << current
          current = []
        else
          current << token
        end
      end
      parts << current
      parts
    end

    def test_path(path)
      File.expand_path(path.to_s, @shell.cwd)
    end

    def execute_function_definition(command)
      @shell.functions[command.name] = command.body
      @shell.mark_status(0)
    end

    def execute_case(command, foreground:)
      word = @shell.expansion.expand_word(command.word).first.to_s

      command.arms.each do |patterns, body|
        matched = patterns.any? do |pattern|
          File.fnmatch(@shell.expansion.expand_word(pattern).first.to_s, word, File::FNM_DOTMATCH)
        end
        return execute_list(body, foreground: foreground) if matched
      end

      0
    end

    def execute_redirected(wrapped, foreground:)
      redirections = build_redirections(wrapped.redirects)
      return @shell.mark_status(1) unless redirections

      fd_map, files = redirections
      begin
        with_redirections(fd_map, files) do
          execute_command(wrapped.command, foreground: foreground)
        end
      ensure
        close_files(files)
      end
    end

    def execute_simple(simple, foreground:)
      @shell.command_substitution_status = nil
      values = {}
      simple.assignments.each do |assignment|
        if assignment.array
          elements = assignment.array.flat_map { |word| @shell.expansion.expand_word(word) }
          @shell.set_array(assignment.name, elements, append: assignment.append)
        elsif assignment.index
          @shell.set_array_element(
            assignment.name,
            assignment.index,
            @shell.expansion.expand_assignment(assignment.value)
          )
        else
          values[assignment.name] = @shell.expansion.expand_assignment(assignment.value)
        end
      end
      words = @shell.expansion.expand_words(simple.words)
      words = expand_aliases(words)
      @shell.stderr.puts("+ #{words.join(" ")}") if @shell.option?(:xtrace) && !words.empty?

      redirections = build_redirections(simple.redirects)
      return @shell.mark_status(1) unless redirections

      fd_map, files = redirections
      if words.empty?
        values.each { |name, value| @shell.export(name, value) }
        close_files(files)
        return @shell.mark_status(@shell.command_substitution_status || 0)
      end

      name = words.first
      argv = words.drop(1)
      builtin = Builtins.lookup(name)
      function = nil
      function_error = nil
      begin
        function = function_for(name)
      rescue StandardError, ScriptError => error
        function_error = error
      end
      ruby_builtin = name == "ruby"

      status =
        if function_error
          report_function_error(name, function_error)
        elsif function.is_a?(AST::List)
          begin
            @shell.with_positional(argv) do
              with_assignments(values) do
                with_redirections(fd_map, files) do
                  execute_list(function, foreground: foreground)
                end
              end
            end
          rescue ControlFlow::Return => signal
            signal.status
          end
        elsif function
          execute_function(name, function, argv, values, fd_map, files)
        elsif builtin
          with_assignments(values) do
            with_redirections(fd_map, files) do
              builtin.call(@shell, argv, @shell.stdin)
            end
          end
        elsif ruby_builtin
          execute_ruby(argv, values, fd_map, files)
        else
          run_external(name, argv, @shell.env.merge(values), fd_map, files,
                       command: words.join(" "), foreground: foreground)
        end

      @shell.clear_pending_signal if builtin || ruby_builtin
      @shell.mark_status(status)
    ensure
      close_files(files) if files
    end

    def function_for(name)
      runtime = @shell.runtime
      @shell.functions[name] || runtime.commands[name] || runtime.commands[name.to_sym] || Functions.load(name, runtime)
    end

    def execute_function(name, block, argv, values, fd_map, files)
      previous_runtime = Sharky.runtime
      previous_shell = @shell.runtime.shell
      previous_io = [$stdin, $stdout, $stderr]
      Sharky.runtime = @shell.runtime
      @shell.runtime.shell = @shell

      status = with_assignments(values) do
        with_process_state do
          with_redirections(fd_map, files) do
            $stdin = @shell.stdin
            $stdout = @shell.stdout
            $stderr = @shell.stderr
            result = block.call(argv, @shell.stdin)
            result.is_a?(Integer) ? result : 0
          end
        end
      end
      @shell.stdout.flush if @shell.stdout.respond_to?(:flush)
      status
    rescue ControlFlow::Return => signal
      signal.status
    rescue StandardError, ScriptError => error
      report_function_error(name, error)
    ensure
      $stdin, $stdout, $stderr = previous_io
      Sharky.runtime = previous_runtime
      @shell.runtime.shell = previous_shell if @shell.runtime
    end

    def execute_ruby(argv, values, fd_map, files)
      previous_io = [$stdin, $stdout, $stderr]
      with_assignments(values) do
        with_redirections(fd_map, files) do
          $stdin = @shell.stdin
          $stdout = @shell.stdout
          $stderr = @shell.stderr
          Repl.start(@shell, argv: argv)
        end
      end
    ensure
      $stdin, $stdout, $stderr = previous_io
    end

    def report_function_error(name, error)
      @shell.stderr.puts("sharky: function #{name} failed: #{error.class}: #{error.message}")
      if @shell.env["SHARKY_DEBUG"] == "1"
        Array(error.backtrace).each { |line| @shell.stderr.puts(line) }
      end
      1
    end

    def expand_aliases(words)
      seen = {}
      loop do
        name = words.first
        break unless name
        break if seen[name]

        value = @shell.runtime.aliases[name]
        break unless value

        seen[name] = true
        parts = begin
          Shellwords.split(value)
        rescue ArgumentError
          break
        end
        break if parts.empty?

        words = parts + words.drop(1)
      end
      words
    end

    def execute_subshell(subshell, foreground:)
      control = job_control?(foreground)
      out_reader, out_writer = capture_pipe(@shell.stdout)
      err_reader, err_writer = capture_pipe(@shell.stderr)

      pid = fork do
        out_reader&.close
        err_reader&.close
        reset_child_signals
        set_child_pgid(0) if control
        enter_shell_cwd
        stdin_io = real_io?(@shell.stdin) ? @shell.stdin : nil
        stdout_io = out_writer || @shell.stdout
        stderr_io = err_writer || @shell.stderr

        setup_child_streams(stdin_io, stdout_io, stderr_io)
        @shell.stdin = stdin_io if stdin_io
        @shell.stdout = stdout_io
        @shell.stderr = stderr_io

        status = execute_list(subshell.list, foreground: false)
        flush_shell_streams
        exit!(status)
      end

      if control
        Process.setpgid(pid, pid) rescue nil
        @shell.track_foreground(pid)
        @shell.give_terminal(pid)
        @shell.forward_pending_signal
      end

      close_io(out_writer)
      close_io(err_writer)
      out_thread = capture_thread(out_reader)
      err_thread = capture_thread(err_reader)
      status = if control
                 wait_foreground([pid], command: command_text_for(subshell), pgid: pid)
               else
                 _, process_status = Process.waitpid2(pid)
                 decode_status(process_status)
               end
      copy_output(out_thread, err_thread)
      status
    end

    def execute_pipeline_commands(commands, foreground:)
      control = job_control?(foreground)
      pipes = Array.new(commands.length - 1) { IO.pipe }
      out_reader, out_writer = capture_pipe(@shell.stdout)
      err_reader, err_writer = capture_pipe(@shell.stderr)
      extras = pipes.flatten + [out_reader, out_writer, err_reader, err_writer].compact
      pids = []
      pgid = nil

      commands.each_with_index do |command, index|
        stdin_io = if index.zero?
                     real_io?(@shell.stdin) ? @shell.stdin : nil
                   else
                     pipes[index - 1][0]
                   end
        stdout_io = index == commands.length - 1 ? (out_writer || @shell.stdout) : pipes[index][1]
        stderr_io = err_writer || @shell.stderr

        child_pgid = control ? (pgid || 0) : nil
        pid = fork_pipeline_component(command, stdin_io, stdout_io, stderr_io, extras,
                                      pgid: child_pgid, job_control: control)
        if control
          pgid ||= pid
          Process.setpgid(pid, pgid) rescue nil
          @shell.track_foreground(pgid) if pids.empty?
        end
        pids << pid

        close_io(pipes[index - 1][0]) if index.positive?
        close_io(pipes[index][1]) if index < commands.length - 1
      end

      if control
        @shell.give_terminal(pgid)
        @shell.forward_pending_signal
      end

      close_io(out_writer)
      close_io(err_writer)
      out_thread = capture_thread(out_reader)
      err_thread = capture_thread(err_reader)
      status = if control
                 wait_foreground(pids, command: command_text_for_pipeline(commands), pgid: pgid)
               else
                 pipeline_status(pids.map { |pid| Process.waitpid2(pid).last })
               end
      copy_output(out_thread, err_thread)
      status
    ensure
      pipes&.flatten&.each { |io| close_io(io) }
      close_io(out_reader)
      close_io(err_reader)
      close_io(out_writer)
      close_io(err_writer)
    end

    def fork_pipeline_component(command, stdin_io, stdout_io, stderr_io, extras, pgid: nil, job_control: false)
      keep = [stdin_io, stdout_io, stderr_io].compact

      fork do
        reset_child_signals
        set_child_pgid(pgid) if job_control
        setup_child_streams(stdin_io, stdout_io, stderr_io)
        @shell.stdin = stdin_io if stdin_io
        @shell.stdout = stdout_io
        @shell.stderr = stderr_io

        extras.each do |io|
          close_io(io) unless keep.any? { |kept| kept.equal?(io) }
        end

        enter_shell_cwd
        status = execute_command(command, foreground: false)
        flush_shell_streams
        exit!(status)
      end
    end

    def start_async(and_or)
      command = command_text(and_or)

      pid = fork do
        begin
          Process.setpgid(0, 0)
        rescue SystemCallError
          nil
        end
        reset_child_signals
        setup_background_streams
        enter_shell_cwd
        status = execute_and_or(and_or, foreground: false)
        flush_shell_streams
        exit!(status)
      end

      begin
        Process.setpgid(pid, pid)
      rescue SystemCallError
        nil
      end

      job = @shell.jobs.add(command: command, pgid: pid, pids: [pid])
      @shell.last_bg_pid = pid
      @shell.stdout.puts("[#{job.id}] #{pid}")
      @shell.mark_status(0)
    end

    def run_external(name, argv, env, fd_map, files, command:, foreground:)
      control = job_control?(foreground)
      out_reader, out_writer = capture_pipe(@shell.stdout)
      err_reader, err_writer = capture_pipe(@shell.stderr)

      pid = fork do
        out_reader&.close
        err_reader&.close
        reset_child_signals
        set_child_pgid(0) if control
        redirect_standard_streams(out_writer, err_writer)
        apply_fd_map(fd_map, out_writer, err_writer)
        enter_shell_cwd
        close_files(files)
        close_io(out_writer)
        close_io(err_writer)

        begin
          $stdout.flush
          $stderr.flush
          reject_unexecutable_format(name, env)
          exec(env, [name, name], *argv)
        rescue Errno::ENOENT
          $stderr.puts("sharky: command not found: #{name}")
          exit!(127)
        rescue Errno::EACCES
          $stderr.puts("sharky: cannot execute: #{name}: permission denied")
          exit!(126)
        rescue Errno::ENOEXEC
          path = executable_path(name, env) || name
          $stderr.puts("sharky: cannot execute: #{name}: no usable interpreter; run bash #{Shellwords.escape(path)} or add a shebang")
          exit!(126)
        end
      end

      if control
        Process.setpgid(pid, pid) rescue nil
        @shell.track_foreground(pid)
        @shell.give_terminal(pid)
        @shell.forward_pending_signal
      end

      close_files(files)
      close_io(out_writer)
      close_io(err_writer)
      out_thread = capture_thread(out_reader)
      err_thread = capture_thread(err_reader)
      status = if control
                 wait_foreground([pid], command: command, pgid: pid)
               else
                 _, process_status = Process.waitpid2(pid)
                 decode_status(process_status)
               end
      copy_output(out_thread, err_thread)
      status
    end

    def reject_unexecutable_format(name, env)
      path = executable_path(name, env)
      return unless path

      header = File.binread(path, 512)
      return if header.start_with?("#!") || elf_for_this_host?(header)

      problem = if header.include?("\0")
                  "unsupported executable format"
                else
                  "no shebang"
                end
      $stderr.puts("sharky: cannot execute: #{name}: #{problem}; run bash #{Shellwords.escape(path)} or add a shebang")
      $stderr.flush
      exit!(126)
    rescue SystemCallError
      nil
    end

    def elf_for_this_host?(header)
      return false unless header.start_with?("\x7FELF".b)
      return false if header.bytesize < 20

      byte_order = header.getbyte(5)
      machine = case byte_order
                when 1 then header.byteslice(18, 2).unpack1("v")
                when 2 then header.byteslice(18, 2).unpack1("n")
                else return false
                end
      expected = host_elf_machine
      expected.nil? || machine == expected
    end

    def host_elf_machine
      case RbConfig::CONFIG["host_cpu"]
      when /\Ax86_64\z/, /\Aamd64\z/ then 62
      when /\Aaarch64\z/, /\Aarm64\z/ then 183
      when /\Ai[3-6]86\z/ then 3
      when /\Aarm/ then 40
      when /\Ariscv64\z/ then 243
      when /\Appc64/ then 21
      when /\As390x\z/ then 22
      end
    end

    def executable_path(name, env)
      if name.include?("/")
        path = File.expand_path(name)
        return path if File.file?(path) && File.executable?(path)

        return nil
      end

      env["PATH"].to_s.split(File::PATH_SEPARATOR).each do |directory|
        directory = Dir.pwd if directory.empty?
        path = File.join(directory, name)
        return path if File.file?(path) && File.executable?(path)
      end
      nil
    end

    def build_redirections(redirects)
      fd_map = {}
      files = []

      expanded = redirects.map do |redirect|
        target = case redirect.op
                 when :dup then redirect.target
                 when :heredoc then heredoc_body(redirect.target)
                 when :here_string then "#{@shell.expansion.expand_assignment(redirect.target)}\n"
                 else File.expand_path(@shell.expansion.expand_assignment(redirect.target), @shell.cwd)
                 end
        [redirect, target]
      end

      expanded.each do |redirect, target|
        if redirect.op == :dup
          target = fd_map[redirect.target] || base_stream(redirect.target)
          unless target
            close_files(files)
            @shell.stderr.puts("sharky: unsupported file descriptor: #{redirect.target}")
            return nil
          end
          fd_map[redirect.fd] = target
          next
        end

        if [:heredoc, :here_string].include?(redirect.op)
          file = Tempfile.new("sharky-redirect")
          files << file
          file.write(target)
          file.rewind
          fd_map[redirect.fd || 0] = file
          next
        end

        path = target
        file = case redirect.op
               when :out
                 File.open(path, File::WRONLY | File::CREAT | File::TRUNC, 0o666)
               when :append
                 File.open(path, File::WRONLY | File::CREAT | File::APPEND, 0o666)
               when :in
                 File.open(path, File::RDONLY)
               when :both
                 File.open(path, File::WRONLY | File::CREAT | File::TRUNC, 0o666)
               else
                 close_files(files)
                 @shell.stderr.puts("sharky: unsupported redirect: #{redirect.op}")
                 return nil
               end

        files << file
        if redirect.op == :both
          fd_map[1] = file
          fd_map[2] = file
        else
          fd_map[redirect.fd] = file
        end
      end

      complete = true
      [fd_map, files]
    rescue SystemCallError => error
      @shell.stderr.puts("sharky: #{error.message}")
      nil
    ensure
      close_files(files) unless complete
    end

    def heredoc_body(document)
      return document.body.to_s if document.quoted

      expand_heredoc(document.body.to_s)
    end

    def expand_heredoc(text)
      escaped = text.gsub("\\", "\\\\\\\\").gsub('"', '\\"')
      word = Lexer.new(%Q{"#{escaped}"}).tokenize.first&.value
      return text unless word

      @shell.expansion.expand_word(word, quoted: true).first.to_s
    rescue LexError
      text
    end

    def with_process_state
      previous_env = ENV.to_h
      previous_dir = Dir.pwd
      start_dir = nil
      baseline = {}
      begin
        ENV.replace(@shell.env)
        Dir.chdir(@shell.cwd)
        start_dir = @shell.cwd
        baseline = ENV.to_h
        yield
      ensure
        reconcile_process_state(start_dir, baseline) if start_dir
        ENV.replace(previous_env)
        begin
          Dir.chdir(previous_dir)
        rescue SystemCallError
          nil
        end
      end
    end

    def reconcile_process_state(start_dir, baseline)
      current_dir = Dir.pwd
      if current_dir != start_dir
        @shell.cd(current_dir)
        ENV["PWD"] = @shell.cwd
      end

      ENV.each do |key, value|
        @shell.env[key] = value unless baseline[key] == value
      end
      baseline.each_key do |key|
        @shell.env.delete(key) unless ENV.key?(key)
      end
    rescue SystemCallError, IOError
      nil
    end

    def with_assignments(values)
      saved = {}
      values.each do |name, value|
        saved[name] = @shell.env[name]
        @shell.env[name] = value
      end
      yield
    ensure
      values.each_key do |name|
        if saved[name]
          @shell.env[name] = saved[name]
        else
          @shell.env.delete(name)
        end
      end
    end

    def with_redirections(fd_map, files)
      original = [@shell.stdin, @shell.stdout, @shell.stderr]
      streams = original.dup

      fd_map.each do |fd, target|
        next unless fd.between?(0, 2)

        streams[fd] = if target.is_a?(Symbol)
                        original[{ stdin: 0, stdout: 1, stderr: 2 }.fetch(target)]
                      else
                        target
                      end
      end

      @shell.stdin = streams[0]
      @shell.stdout = streams[1]
      @shell.stderr = streams[2]
      yield
    ensure
      @shell.stdin, @shell.stdout, @shell.stderr = original
      close_files(files)
    end

    def redirect_standard_streams(out_writer, err_writer)
      $stdin.reopen(@shell.stdin) if real_io?(@shell.stdin)
      $stdout = STDOUT unless real_io?($stdout)
      $stderr = STDERR unless real_io?($stderr)
      $stdout.reopen(out_writer || @shell.stdout)
      $stderr.reopen(err_writer || @shell.stderr)
    end

    def apply_fd_map(fd_map, out_writer, err_writer)
      fd_map.each do |fd, target|
        stream = case target
                 when :stdin then @shell.stdin
                 when :stdout then out_writer || @shell.stdout
                 when :stderr then err_writer || @shell.stderr
                 else target
                 end
        next unless real_io?(stream)

        case fd
        when 0 then $stdin.reopen(stream)
        when 1 then $stdout.reopen(stream)
        when 2 then $stderr.reopen(stream)
        end
      end
    end

    def setup_child_streams(stdin_io, stdout_io, stderr_io)
      $stdin.reopen(stdin_io) if real_io?(stdin_io)
      $stdout.reopen(stdout_io) if real_io?(stdout_io)
      $stderr.reopen(stderr_io) if real_io?(stderr_io)
    end

    def setup_background_streams
      stdout_io = real_io?(@shell.stdout) ? @shell.stdout : File.open(File::NULL, "w")
      stderr_io = real_io?(@shell.stderr) ? @shell.stderr : File.open(File::NULL, "w")
      stdin_io = real_io?(@shell.stdin) ? @shell.stdin : File.open(File::NULL, "r")

      $stdout.reopen(stdout_io)
      $stderr.reopen(stderr_io)
      $stdin.reopen(stdin_io)
      @shell.stdout = stdout_io
      @shell.stderr = stderr_io
    end

    def capture_pipe(stream)
      return [nil, nil] if real_io?(stream)

      IO.pipe
    end

    def capture_thread(reader)
      return nil unless reader

      Thread.new do
        begin
          reader.read
        ensure
          close_io(reader)
        end
      end
    end

    def copy_output(out_thread, err_thread)
      out = out_thread&.value.to_s
      err = err_thread&.value.to_s
      @shell.stdout.write(out) unless out.empty?
      @shell.stderr.write(err) unless err.empty?
    end

    def close_files(files)
      files.each { |file| close_io(file) }
    end

    def close_io(io)
      io.close unless io.nil? || io.closed?
    rescue IOError
      nil
    end

    def flush_shell_streams
      @shell.stdout.flush if @shell.stdout.respond_to?(:flush)
      @shell.stderr.flush if @shell.stderr.respond_to?(:flush)
    rescue IOError
      nil
    end

    def job_control?(foreground)
      foreground && @shell.job_control? && @shell.terminal&.tty?
    end

    def set_child_pgid(pgid)
      Process.setpgid(0, pgid)
    rescue SystemCallError
      nil
    end

    def reset_child_signals
      %w[INT QUIT TSTP TTOU TTIN CHLD WINCH].each do |signal|
        next unless Signal.list.key?(signal)

        Signal.trap(signal, "DEFAULT")
      rescue ArgumentError
        nil
      end
    end

    def wait_foreground(pids, command:, pgid:)
      statuses = pids.map do |pid|
        _, status = Process.waitpid2(pid, Process::WUNTRACED)
        status
      end
      stopped = statuses.find(&:stopped?)
      if stopped
        @shell.jobs.add(command: command, pgid: pgid, pids: pids, state: :stopped)
        @shell.take_terminal
        return 128 + stopped.stopsig
      end

      @shell.take_terminal
      pipeline_status(statuses)
    rescue Errno::ECHILD
      @shell.take_terminal
      1
    end

    def command_text_for_pipeline(commands)
      commands.map { |command| command_text_for(command) }.join(" | ")
    end

    def command_text(and_or)
      and_or.first.commands.map { |command| command_text_for(command) }.join(" | ")
    end

    def command_text_for(command)
      case command
      when AST::Simple
        text = command.words.map { |word| word_text(word) }.join(" ")
        redirects = command.redirects.map { |redirect| redirect_text(redirect) }.join(" ")
        [text, redirects].reject(&:empty?).join(" ")
      when AST::Subshell
        "(#{list_text(command.list)})"
      when AST::Redirected
        [command_text_for(command.command), *command.redirects.map { |r| redirect_text(r) }]
          .reject(&:empty?).join(" ")
      when AST::If
        branches = command.branches
        condition, body = branches.first
        text = "if #{list_text(condition)}; then #{list_text(body)}"
        branches.drop(1).each do |extra_condition, extra_body|
          text << "; elif #{list_text(extra_condition)}; then #{list_text(extra_body)}"
        end
        text << "; else #{list_text(command.else_body)}" if command.else_body
        "#{text}; fi"
      when AST::For
        words = command.words.map { |word| word_text(word) }
        header = words.empty? ? "for #{command.name}" : "for #{command.name} in #{words.join(' ')}"
        "#{header}; do #{list_text(command.body)}; done"
      when AST::While
        keyword = command.until_form ? "until" : "while"
        "#{keyword} #{list_text(command.condition)}; do #{list_text(command.body)}; done"
      when AST::Case
        arms = command.arms.map do |patterns, body|
          "#{patterns.map { |pattern| word_text(pattern) }.join('|')}) #{list_text(body)};;"
        end
        "case #{word_text(command.word)} in #{arms.join(' ')} esac"
      when AST::Group
        "{ #{list_text(command.list)}; }"
      when AST::FunctionDef
        "#{command.name}() { #{list_text(command.body)}; }"
      when AST::Arithmetic
        "((#{command.source}))"
      when AST::TestCommand
        "[[ #{command.source} ]]"
      when AST::Not
        "! #{command_text_for(command.command)}"
      else
        ""
      end
    end

    def list_text(list)
      return "" unless list

      list.entries.map do |_kind, and_ors|
        and_or = and_ors.first
        and_or ? command_text(and_or) : ""
      end.reject(&:empty?).join("; ")
    end

    def word_text(word)
      word.parts.map do |part|
        case part.type
        when :literal, :tilde then part.value.to_s
        when :single then "'#{part.value}'"
        when :double then "\"#{part.value.map { |child| word_text(AST::Word.new(parts: [child])) }.join}\""
        when :variable, :special, :positional then "$#{part.value}"
        when :parameter then "${#{part.value.name}#{part.value.index ? "[#{part.value.index}]" : ""}}"
        when :arithmetic then "$((#{part.value.source}))"
        when :cmdsub then "$(...)"
        else part.value.to_s
        end
      end.join
    end

    def redirect_text(redirect)
      operator = {
        out: ">", append: ">>", in: "<", both: "&>", heredoc: "<<",
        heredoc_strip: "<<-", here_string: "<<<", dup: ">&"
      }.fetch(redirect.op, redirect.op.to_s)
      target = redirect.target.respond_to?(:delimiter) ? redirect.target.delimiter : word_text(redirect.target)
      "#{redirect.fd == 0 ? "" : redirect.fd}#{operator} #{target}".strip
    end

    def enter_shell_cwd
      Dir.chdir(@shell.cwd)
    rescue SystemCallError => error
      $stderr.puts("sharky: #{error.message}")
      $stderr.flush
      exit!(1)
    end

    def real_io?(stream)
      stream.respond_to?(:fileno) && stream.fileno
    rescue IOError, NotImplementedError
      false
    end

    def pipeline_status(statuses)
      codes = statuses.map { |status| decode_status(status) }
      @shell.option?(:pipefail) ? (codes.reverse.find { |code| !code.zero? } || 0) : codes.last
    end

    def decode_status(status)
      return 1 if status.nil?
      return 128 + status.termsig if status.signaled?

      status.exitstatus || 1
    end

    def base_stream(fd)
      { 0 => :stdin, 1 => :stdout, 2 => :stderr }[fd]
    end

    def unsupported
      @shell.stderr.puts("sharky: unsupported command")
      @shell.mark_status(2)
    end
  end
end

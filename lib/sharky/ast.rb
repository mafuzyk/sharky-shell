module Sharky
  module AST
    Program = Struct.new(:lines, keyword_init: true)
    Line = Struct.new(:list, keyword_init: true)
    List = Struct.new(:entries, keyword_init: true)
    AndOr = Struct.new(:first, :rest, keyword_init: true) do
      def commands
        first.commands
      end
    end
    Pipeline = Struct.new(:commands, keyword_init: true)
    Not = Struct.new(:command, :pos, keyword_init: true)
    Simple = Struct.new(:assignments, :words, :redirects, keyword_init: true)
    Subshell = Struct.new(:list, keyword_init: true)
    Assignment = Struct.new(:name, :value, keyword_init: true)
    Redirect = Struct.new(:fd, :op, :target, keyword_init: true)
    Word = Struct.new(:parts, :pos, keyword_init: true)
    Part = Struct.new(:type, :value, :pos, keyword_init: true)
    Parameter = Struct.new(:name, :operator, :argument, :pos, keyword_init: true)
    If = Struct.new(:branches, :else_body, :pos, keyword_init: true)
    For = Struct.new(:name, :words, :body, :pos, keyword_init: true)
    While = Struct.new(:condition, :body, :until_form, :pos, keyword_init: true)
    Case = Struct.new(:word, :arms, :pos, keyword_init: true)
    FunctionDef = Struct.new(:name, :body, :pos, keyword_init: true)
    Group = Struct.new(:list, :pos, keyword_init: true)
    Arithmetic = Struct.new(:source, :pos, keyword_init: true)
    TestCommand = Struct.new(:source, :double_bracket, :pos, keyword_init: true)
    HereDoc = Struct.new(:delimiter, :strip_tabs, :quoted, :body, :pos, keyword_init: true)
  end
end

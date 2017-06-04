//! Functions for converting an AST to TIR.
use std::rc::Rc;
use std::fs::File;
use std::io::Read;
use std::path::MAIN_SEPARATOR;
use std::collections::HashMap;

use compiler::diagnostics::Diagnostics;
use config::Config;
use parser::{Parser, Node};
use tir::code_object::CodeObject;
use tir::expression::Expression;
use tir::implement::{Implement, Rename};
use tir::import::Symbol as ImportSymbol;
use tir::method::MethodArgument;
use tir::module::Module;
use tir::variable::{Mutability, Scope as VariableScope, Variable};

pub struct Builder {
    pub config: Rc<Config>,

    /// Any diagnostics that were produced when compiling modules.
    pub diagnostics: Diagnostics,

    /// All the compiled modules, mapped to their names. The values of this hash
    /// are explicitly set to None when:
    ///
    /// * The module was found and is about to be processed for the first time
    /// * The module could not be found
    ///
    /// This prevents recursive imports from causing the compiler to get stuck
    /// in a loop.
    pub modules: HashMap<String, Option<Module>>,
}

struct Context<'a> {
    /// The path of the module that is being compiled.
    path: &'a String,

    /// The local variables for the current scope.
    locals: &'a mut VariableScope,

    /// The module locals for the currently compiled module.
    globals: &'a mut VariableScope,
}

impl Builder {
    pub fn new(config: Rc<Config>) -> Self {
        Builder {
            config: config,
            diagnostics: Diagnostics::new(),
            modules: HashMap::new(),
        }
    }

    pub fn build(&mut self, path: String) -> Option<Module> {
        let module = if let Ok(ast) = self.parse_file(&path) {
            let mut globals = VariableScope::new();
            let code_object = self.code_object(&path, &ast, &mut globals);
            let mod_name = self.module_name_for_path(&path);

            let module = Module {
                path: path,
                name: mod_name,
                code: code_object,
                globals: globals,
            };

            Some(module)
        } else {
            None
        };

        module
    }

    fn code_object(&mut self,
                   path: &String,
                   node: &Node,
                   globals: &mut VariableScope)
                   -> CodeObject {
        self.code_object_with_locals(path, node, VariableScope::new(), globals)
    }

    fn code_object_with_locals(&mut self,
                               path: &String,
                               node: &Node,
                               mut locals: VariableScope,
                               globals: &mut VariableScope)
                               -> CodeObject {
        let body = match node {
            &Node::Expressions { ref nodes } => {
                let mut context = Context {
                    path: path,
                    locals: &mut locals,
                    globals: globals,
                };

                self.process_nodes(nodes, &mut context)
            }
            _ => Vec::new(),
        };

        CodeObject { locals: locals, body: body }
    }

    fn process_nodes(&mut self,
                     nodes: &Vec<Node>,
                     context: &mut Context)
                     -> Vec<Expression> {
        nodes.iter()
            .map(|ref node| self.process_node(node, context))
            .collect()
    }

    fn process_node(&mut self, node: &Node, context: &mut Context) -> Expression {
        match node {
            &Node::Integer { value, line, column } => {
                self.integer(value, line, column)
            }
            &Node::Float { value, line, column } => {
                self.float(value, line, column)
            }
            &Node::String { ref value, line, column } => {
                self.string(value.clone(), line, column)
            }
            &Node::Array { ref values, line, column } => {
                self.array(values, line, column, context)
            }
            &Node::Hash { ref pairs, line, column } => {
                self.hash(pairs, line, column, context)
            }
            &Node::SelfObject { line, column } => self.get_self(line, column),
            &Node::Identifier { ref name, line, column } => {
                self.identifier(name, line, column, context)
            }
            &Node::Attribute { ref name, line, column } => {
                self.attribute(name.clone(), line, column)
            }
            &Node::Constant { ref receiver, ref name, line, column } => {
                self.get_constant(name.clone(), receiver, line, column, context)
            }
            &Node::Type { ref constant, .. } => {
                // TODO: actually use type information
                self.process_node(constant, context)
            }
            &Node::LetDefine { ref name, ref value, line, column, .. } => {
                self.set_variable(name,
                                  value,
                                  Mutability::Immutable,
                                  line,
                                  column,
                                  context)
            }
            &Node::VarDefine { ref name, ref value, line, column, .. } => {
                self.set_variable(name,
                                  value,
                                  Mutability::Mutable,
                                  line,
                                  column,
                                  context)
            }
            &Node::Send { ref name,
                          ref receiver,
                          ref arguments,
                          line,
                          column } => {
                self.send_object_message(name.clone(),
                                         receiver,
                                         arguments,
                                         line,
                                         column,
                                         context)
            }
            &Node::Import { ref steps, ref symbols, line, column } => {
                self.import(steps, symbols, line, column, context)
            }
            &Node::Closure { ref arguments, ref body, line, column, .. } => {
                self.closure(arguments, body, line, column, context)
            }
            &Node::KeywordArgument { ref name, ref value, line, column } => {
                self.keyword_argument(name.clone(), value, line, column, context)
            }
            &Node::Method { ref name,
                            ref receiver,
                            ref arguments,
                            ref body,
                            ref requirements,
                            line,
                            column,
                            .. } => {
                if let &Some(ref body) = body {
                    self.method(name.clone(),
                                receiver,
                                arguments,
                                requirements,
                                body,
                                line,
                                column,
                                context)
                } else {
                    self.required_method(name.clone(),
                                         receiver,
                                         arguments,
                                         requirements,
                                         line,
                                         column,
                                         context)
                }
            }
            &Node::Class { ref name,
                           ref implements,
                           ref body,
                           line,
                           column,
                           .. } => {
                self.class(name.clone(), implements, body, line, column, context)
            }
            &Node::Trait { ref name, ref body, line, column, .. } => {
                self.def_trait(name.clone(), body, line, column, context)
            }
            &Node::Return { ref value, line, column } => {
                self.return_value(value, line, column, context)
            }
            &Node::TypeCast { ref value, .. } => self.type_cast(value, context),
            &Node::Try { ref body,
                         ref else_body,
                         ref else_argument,
                         line,
                         column,
                         .. } => {
                self.try(body, else_body, else_argument, line, column, context)
            }
            _ => Expression::Void,
        }
    }

    fn integer(&self, val: i64, line: usize, col: usize) -> Expression {
        Expression::Integer {
            value: val,
            line: line,
            column: col,
        }
    }

    fn float(&self, val: f64, line: usize, col: usize) -> Expression {
        Expression::Float {
            value: val,
            line: line,
            column: col,
        }
    }

    fn string(&self, val: String, line: usize, col: usize) -> Expression {
        Expression::String {
            value: val,
            line: line,
            column: col,
        }
    }

    fn array(&mut self,
             value_nodes: &Vec<Node>,
             line: usize,
             col: usize,
             context: &mut Context)
             -> Expression {
        let values = self.process_nodes(&value_nodes, context);

        Expression::Array {
            values: values,
            line: line,
            column: col,
        }
    }

    fn hash(&mut self,
            pair_nodes: &Vec<(Node, Node)>,
            line: usize,
            col: usize,
            context: &mut Context)
            -> Expression {
        let pairs = pair_nodes.iter()
            .map(|&(ref k, ref v)| {
                (self.process_node(k, context), self.process_node(v, context))
            })
            .collect();

        Expression::Hash {
            pairs: pairs,
            line: line,
            column: col,
        }
    }

    fn get_self(&self, line: usize, col: usize) -> Expression {
        Expression::GetSelf { line: line, column: col }
    }

    fn identifier(&mut self,
                  name: &String,
                  line: usize,
                  col: usize,
                  context: &mut Context)
                  -> Expression {
        // TODO: look up methods before looking up globals
        if let Some(local) = context.locals.lookup(name) {
            return self.get_local(local, line, col);
        }

        if let Some(global) = context.globals.lookup(name) {
            return self.get_global(global, line, col);
        }

        // TODO: check if the method actually exists.
        let args = Vec::new();

        self.send_object_message(name.clone(), &None, &args, line, col, context)
    }

    fn attribute(&mut self, name: String, line: usize, col: usize) -> Expression {
        Expression::GetAttribute {
            receiver: Box::new(self.get_self(line, col)),
            name: name,
            line: line,
            column: col,
        }
    }

    fn get_local(&mut self,
                 variable: Variable,
                 line: usize,
                 col: usize)
                 -> Expression {
        Expression::GetLocal {
            variable: variable,
            line: line,
            column: col,
        }
    }

    fn get_global(&mut self,
                  variable: Variable,
                  line: usize,
                  col: usize)
                  -> Expression {
        Expression::GetGlobal {
            variable: variable,
            line: line,
            column: col,
        }
    }

    fn get_constant(&mut self,
                    name: String,
                    receiver: &Option<Box<Node>>,
                    line: usize,
                    col: usize,
                    context: &mut Context)
                    -> Expression {
        let rec_expr = if let &Some(ref node) = receiver {
            self.process_node(node, context)
        } else {
            self.get_self(line, col)
        };

        Expression::GetAttribute {
            receiver: Box::new(rec_expr),
            name: name,
            line: line,
            column: col,
        }
    }

    fn set_constant(&mut self,
                    name: String,
                    value: Expression,
                    line: usize,
                    col: usize)
                    -> Expression {
        let self_expr = self.get_self(line, col);

        Expression::SetAttribute {
            receiver: Box::new(self_expr),
            name: name,
            value: Box::new(value),
            line: line,
            column: col,
        }
    }

    fn set_variable(&mut self,
                    name_node: &Node,
                    value_node: &Node,
                    mutability: Mutability,
                    line: usize,
                    column: usize,
                    context: &mut Context)
                    -> Expression {
        let value_expr = self.process_node(value_node, context);

        match name_node {
            &Node::Identifier { ref name, .. } => {
                self.set_local(name.clone(),
                               value_expr,
                               mutability,
                               line,
                               column,
                               context)
            }
            &Node::Constant { ref name, .. } => {
                if mutability == Mutability::Mutable {
                    self.diagnostics.error(context.path,
                                           "constants can not be declared as \
                                            mutable",
                                           line,
                                           column);
                }

                self.set_constant(name.clone(), value_expr, line, column)
            }
            &Node::Attribute { ref name, .. } => {
                self.set_attribute(name.clone(), value_expr, line, column)
            }
            _ => unreachable!(),
        }
    }

    fn set_local(&mut self,
                 name: String,
                 value: Expression,
                 mutability: Mutability,
                 line: usize,
                 col: usize,
                 context: &mut Context)
                 -> Expression {
        Expression::SetLocal {
            variable: context.locals.define(name, mutability),
            value: Box::new(value),
            line: line,
            column: col,
        }
    }

    fn set_attribute(&self,
                     name: String,
                     value: Expression,
                     line: usize,
                     col: usize)
                     -> Expression {
        // TODO: track mutability of attributes per receiver type
        Expression::SetAttribute {
            receiver: Box::new(self.get_self(line, col)),
            name: name,
            value: Box::new(value),
            line: line,
            column: col,
        }
    }

    fn send_object_message(&mut self,
                           name: String,
                           receiver_node: &Option<Box<Node>>,
                           arguments: &Vec<Node>,
                           line: usize,
                           col: usize,
                           context: &mut Context)
                           -> Expression {
        let receiver = if let &Some(ref rec) = receiver_node {
            self.process_node(rec, context)
        } else {
            self.get_self(line, col)
        };

        let mut args = vec![receiver.clone()];

        for arg in arguments.iter() {
            args.push(self.process_node(arg, context));
        }

        Expression::SendObjectMessage {
            receiver: Box::new(receiver),
            name: name,
            arguments: args,
            line: line,
            column: col,
        }
    }

    /// Converts the list of import steps to a module name.
    fn module_name_for_import(&self, steps: &Vec<Node>) -> String {
        let mut chunks = Vec::new();

        for step in steps.iter() {
            match step {
                &Node::Identifier { ref name, .. } => {
                    chunks.push(name.clone());
                }
                &Node::Constant { .. } => break,
                _ => {}
            }
        }

        chunks.join(self.config.lookup_separator())
    }

    /// Returns a vector of symbols to import, based on a list of AST nodes
    /// describing the import steps.
    fn import_symbols(&self,
                      nodes: &Vec<Node>,
                      context: &mut Context)
                      -> Vec<ImportSymbol> {
        let mut symbols = Vec::new();

        for node in nodes.iter() {
            match node {
                &Node::ImportSymbol { symbol: ref symbol_node,
                                      alias: ref alias_node } => {
                    let alias = if let &Some(ref node) = alias_node {
                        self.name_of_node(node)
                    } else {
                        None
                    };

                    let func = match **symbol_node {
                        Node::Identifier { .. } => ImportSymbol::module,
                        Node::Constant { .. } => ImportSymbol::constant,
                        _ => unreachable!(),
                    };

                    let symbol = match **symbol_node {
                        Node::Identifier { ref name, line, column } |
                        Node::Constant { ref name, line, column, .. } => {
                            let var_name = if let Some(alias) = alias {
                                alias
                            } else {
                                name.clone()
                            };

                            func(name.clone(),
                                 context.globals
                                     .define(var_name, Mutability::Immutable),
                                 line,
                                 column)
                        }
                        _ => unreachable!(),
                    };

                    symbols.push(symbol);
                }
                _ => {}
            }
        }

        symbols
    }

    fn import(&mut self,
              step_nodes: &Vec<Node>,
              symbol_nodes: &Vec<Node>,
              line: usize,
              col: usize,
              context: &mut Context)
              -> Expression {
        let mod_name = self.module_name_for_import(step_nodes);
        let mod_path = self.module_path_for_name(&mod_name);

        // We insert the module name before processing it to prevent the
        // compiler from getting stuck in a recursive import.
        if self.modules.get(&mod_name).is_none() {
            self.modules.insert(mod_name.clone(), None);

            match self.find_module_path(&mod_path) {
                Some(full_path) => {
                    let module = self.build(full_path);

                    self.modules.insert(mod_name.clone(), module);
                }
                None => {
                    self.diagnostics
                        .error(context.path,
                               format!("The module {:?} could not be found",
                                       mod_name),
                               line,
                               col);

                    return Expression::Void;
                }
            };
        }

        // At this point the value for the current module path is either
        // Some(module) or None.
        if self.modules.get(&mod_name).unwrap().is_some() {
            Expression::ImportModule {
                path: mod_path,
                line: line,
                column: col,
                symbols: self.import_symbols(symbol_nodes, context),
            }
        } else {
            Expression::Void
        }
    }

    fn closure(&mut self,
               arg_nodes: &Vec<Node>,
               body_node: &Node,
               line: usize,
               col: usize,
               context: &mut Context)
               -> Expression {
        let body = self.code_object(&context.path, body_node, context.globals);

        Expression::Closure {
            arguments: self.method_arguments(arg_nodes, context),
            body: body,
            line: line,
            column: col,
        }
    }

    fn keyword_argument(&mut self,
                        name: String,
                        value: &Node,
                        line: usize,
                        col: usize,
                        context: &mut Context)
                        -> Expression {
        Expression::KeywordArgument {
            name: name,
            value: Box::new(self.process_node(value, context)),
            line: line,
            column: col,
        }
    }

    fn method(&mut self,
              name: String,
              receiver: &Option<Box<Node>>,
              arg_nodes: &Vec<Node>,
              requirements: &Vec<Node>,
              body: &Node,
              line: usize,
              col: usize,
              context: &mut Context)
              -> Expression {
        let arguments = self.method_arguments(arg_nodes, context);
        let mut locals = VariableScope::new();

        for arg in arguments.iter() {
            locals.define(arg.name.clone(), Mutability::Immutable);
        }

        let body_expr = self.code_object_with_locals(&context.path,
                                                     body,
                                                     locals,
                                                     context.globals);

        let receiver_expr = receiver.as_ref()
            .map(|ref r| Box::new(self.process_node(r, context)));

        Expression::Method {
            name: name,
            receiver: receiver_expr,
            arguments: arguments,
            body: body_expr,
            line: line,
            column: col,
            requires: self.process_nodes(requirements, context),
        }
    }

    fn required_method(&mut self,
                       name: String,
                       receiver: &Option<Box<Node>>,
                       arguments: &Vec<Node>,
                       requirements: &Vec<Node>,
                       line: usize,
                       col: usize,
                       context: &mut Context)
                       -> Expression {
        if receiver.is_some() {
            self.diagnostics.error(context.path,
                                   "methods required by a trait can not be \
                                    defined on an explicit receiver",
                                   line,
                                   col);
        }

        Expression::RequiredMethod {
            name: name,
            arguments: self.method_arguments(arguments, context),
            line: line,
            column: col,
            requires: self.process_nodes(requirements, context),
        }
    }

    fn method_arguments(&mut self,
                        nodes: &Vec<Node>,
                        context: &mut Context)
                        -> Vec<MethodArgument> {
        nodes.iter()
            .map(|node| match node {
                &Node::ArgumentDefine { ref name,
                                        ref default,
                                        line,
                                        column,
                                        rest,
                                        .. } => {
                    let default_val = default.as_ref()
                        .map(|node| self.process_node(node, context));

                    MethodArgument {
                        name: name.clone(),
                        default_value: default_val,
                        line: line,
                        column: column,
                        rest: rest,
                    }
                }
                _ => unreachable!(),
            })
            .collect()
    }

    fn class(&mut self,
             name: String,
             implements: &Vec<Node>,
             body: &Node,
             line: usize,
             col: usize,
             context: &mut Context)
             -> Expression {
        let code_object = self.code_object(&context.path, body, context.globals);
        let impl_exprs = self.implements(implements, context);

        Expression::Class {
            name: name,
            body: code_object,
            implements: impl_exprs,
            line: line,
            column: col,
        }
    }

    fn def_trait(&mut self,
                 name: String,
                 body: &Node,
                 line: usize,
                 col: usize,
                 context: &mut Context)
                 -> Expression {
        let code_object = self.code_object(&context.path, body, context.globals);

        Expression::Trait {
            name: name,
            body: code_object,
            line: line,
            column: col,
        }
    }

    fn implements(&mut self,
                  nodes: &Vec<Node>,
                  context: &mut Context)
                  -> Vec<Implement> {
        nodes.iter()
            .map(|node| match node {
                &Node::Implement { ref name, ref renames, line, column, .. } => {
                    self.implement(name, renames, line, column, context)
                }
                _ => unreachable!(),
            })
            .collect()
    }

    fn implement(&mut self,
                 name: &Node,
                 rename_nodes: &Vec<(Node, Node)>,
                 line: usize,
                 col: usize,
                 context: &mut Context)
                 -> Implement {
        let renames = rename_nodes.iter()
            .map(|&(ref src, ref alias)| {
                let src_name = self.name_of_node(src).unwrap();
                let alias_name = self.name_of_node(alias).unwrap();

                Rename::new(src_name, alias_name)
            })
            .collect();

        Implement::new(self.process_node(name, context), renames, line, col)
    }

    fn return_value(&mut self,
                    value: &Option<Box<Node>>,
                    line: usize,
                    col: usize,
                    context: &mut Context)
                    -> Expression {
        let ret_val = if let &Some(ref node) = value {
            self.process_node(node, context)
        } else {
            Expression::Nil { line: line, column: col }
        };

        Expression::Return {
            value: Box::new(ret_val),
            line: line,
            column: col,
        }
    }

    fn type_cast(&mut self, value: &Node, context: &mut Context) -> Expression {
        self.process_node(value, context)
    }

    fn try(&mut self,
           body: &Node,
           else_body: &Option<Box<Node>>,
           else_arg: &Option<Box<Node>>,
           line: usize,
           col: usize,
           context: &mut Context)
           -> Expression {
        let body = self.code_object(&context.path, body, context.globals);

        let (else_body, else_arg) = if let &Some(ref node) = else_body {
            let mut else_locals = VariableScope::new();

            let else_arg = if let &Some(ref node) = else_arg {
                let name = self.name_of_node(node).unwrap();

                Some(else_locals.define(name, Mutability::Immutable))
            } else {
                None
            };

            let body = self.code_object_with_locals(&context.path,
                                                    node,
                                                    else_locals,
                                                    context.globals);

            (Some(body), else_arg)
        } else {
            (None, None)
        };

        Expression::Try {
            body: body,
            else_body: else_body,
            else_argument: else_arg,
            line: line,
            column: col,
        }
    }

    fn name_of_node(&self, node: &Node) -> Option<String> {
        match node {
            &Node::Identifier { ref name, .. } |
            &Node::Constant { ref name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    fn parse_file(&mut self, path: &String) -> Result<Node, ()> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(err) => {
                self.diagnostics.error(path, err.to_string(), 1, 1);
                return Err(());
            }
        };

        let mut input = String::new();

        if let Err(err) = file.read_to_string(&mut input) {
            self.diagnostics.error(path, err.to_string(), 1, 1);
            return Err(());
        }

        let mut parser = Parser::new(&input);

        match parser.parse() {
            Ok(ast) => Ok(ast),
            Err(err) => {
                self.diagnostics
                    .error(path, err, parser.line(), parser.column());

                Err(())
            }
        }
    }

    fn module_name_for_path(&self, path: &String) -> String {
        if let Some(file_with_ext) = path.split(MAIN_SEPARATOR).last() {
            if let Some(file_name) = file_with_ext.split(".").next() {
                return file_name.to_string();
            }
        }

        String::new()
    }

    fn module_path_for_name(&self, name: &str) -> String {
        let file_name =
            name.replace(self.config.lookup_separator(),
                         &MAIN_SEPARATOR.to_string());

        file_name + self.config.source_extension()
    }

    fn find_module_path(&self, path: &str) -> Option<String> {
        for dir in self.config.source_directories.iter() {
            let full_path = dir.join(path);

            if full_path.exists() {
                return Some(full_path.to_str().unwrap().to_string());
            }
        }

        None
    }
}
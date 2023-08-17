import { CalculatingEngine } from '../../../../js/modules/Calculator/CalculatingEngine.js';
import History from './History.js';

class Input {
    constructor(calculatorElement) {
        this.operators = ['+', '-', '*', '/', '='];
        this.calculatorElement = calculatorElement;
        this.engine = new CalculatingEngine();
        this.history = new History(this.operators);

        this.setupEventListeners();
    }

    setupEventListeners() {
        this.calculatorElement.querySelector('.numbers').addEventListener('click', this.handleNumberClick.bind(this));
        this.calculatorElement.querySelector('.operators').addEventListener('click', this.handleOperatorClick.bind(this));
        
    }

    handleNumberClick(event) {
        const target = event.target;
        if (target.classList.contains('number')) {
            this.history.addEntry(target.value);
        }
    }

    handleOperatorClick(event) {
        const target = event.target;
        if (target.classList.contains('operator')) {
            this.processOperator(target);
        }
    }

    processOperator(target) {
        if (target.value === 'equal') {
            this.handleEquals();
        } else if (target.value === 'back') {
            this.history.removeLastEntry();
        } else if (target.value === 'allclear') {
            this.handleAllClear();
        } else {
            this.history.addEntry(target.textContent);
        }
    }

    handleAllClear() {
        this.history.clearHistory();
    }

    handleEquals() {
        const currentCalculation = this.history.getCurrentCalculation();
        
        if (currentCalculation.length === 0) return;
    
        if (this.operators.includes(currentCalculation[currentCalculation.length - 1])) {
            for (let i = currentCalculation.length - 2; i >= 0; i--) {
                if (!this.operators.includes(currentCalculation[i])) {
                    this.history.addEntry(currentCalculation[i]);
                    break;
                }
            }
        }
    
        const expressionSinceLastEquals = this.history.getCurrentCalculation();
        if (!expressionSinceLastEquals.every(e => this.operators.includes(e) || e === '=')) {
            const result = this.engine.evaluate(expressionSinceLastEquals.join(''));
            this.history.addEntry('=');
            this.history.addEntry(result);
            this.history.startNewCalculationWithResult(result);
        }
    }
    

    registerDisplayObserver(display) {
        this.history.registerObserver(display);
    }
}

export default Input;